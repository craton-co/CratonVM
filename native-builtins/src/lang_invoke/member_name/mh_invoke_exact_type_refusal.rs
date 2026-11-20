#[allow(unused_imports)]
use super::*;

/// `invokeExact`'s `WrongMethodTypeException` -- `handle's method type
/// {handle_type} but found {call_site}`, each type as `MethodType.toString()`
/// shows it -- when `call_site` is not exactly `handle_type`; `None` when they
/// agree. JIT r23 w4 review (lane L2): the VM's interpreted lambda dispatch
/// raises it for a `REF_invokeSpecial` handle forwarder whose `invokeExact`
/// names the member's declaring class while the handle's receiver is the
/// caller (JDK-8172817).
pub fn mh_invoke_exact_type_refusal(
    ctx: &mut dyn NativeContext,
    handle_type: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    varhandle_exact_mismatch(ctx, handle_type, call_site)
}

/// The `WrongMethodTypeException` of [`varhandle_exact_call_site_refusal`]
/// when `call_site` is not exactly `expected` (an empty `expected` is an
/// unknown type and accepts).
pub fn varhandle_exact_mismatch(
    ctx: &mut dyn NativeContext,
    expected: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    if expected.is_empty() || expected == call_site {
        return None;
    }
    let shown_expected = method_type_display_in(&*ctx, expected)?;
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        &format!("handle's method type {shown_expected} but found {shown_site}"),
    ))
}

/// `(Ljava/lang/invoke/VarHandle;` + `desc`'s parameters + `)R`: an access
/// mode invoker's type for the access type `desc`.
pub fn vh_invoker_descriptor(desc: &str) -> Option<String> {
    let rest = desc.strip_prefix('(')?;
    Some(format!("(L{VAR_HANDLE_CLASS};{rest}"))
}

/// The refusal of a call through a `VarHandle` access mode invoker (an
/// `MH_KIND_VIRTUAL` handle on `VarHandle.<mode>`: `varHandleInvoker`,
/// `varHandleExactInvoker`, a direct handle's mode invoker) at call-site type
/// `desc` on `vh`, before anything is read or written:
///
/// * an exact invoker ([`MH_MARK_EXACT_INVOKER`] on such a handle) refuses a
///   handle whose access mode type is not `desc`, naming both with the
///   leading `VarHandle` (HotSpot's `Invokers.checkVarHandleExactType`):
///   `handle's method type (VarHandle,H)int but found (VarHandle,H)long`;
/// * any other invoker is a plain call site of type `desc`, so an
///   invoke-exact handle refuses a mismatch as `vh.get(..)` would
///   ([`varhandle_exact_call_site_refusal`]; the text has no `VarHandle`).
///
/// Interpreter round i1 wave 50, lane L4 (`L4W49VarHandleInvokers`).
#[cold]
#[inline(never)]
pub fn vh_invoker_call_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    vh: ObjectRef,
    method_name: &str,
    desc: &str,
) -> Option<MethodCallFailed> {
    if !mh_is_exact_invoker(&*ctx, mh) {
        return varhandle_exact_call_site_refusal(ctx, vh, method_name, desc);
    }
    let access_type = vh_access_type_of_method(method_name)?;
    let own = match p67_memory_segment_varhandle_descriptor(ctx, vh, access_type) {
        Some(own) => own,
        None => {
            let (_kind, value_desc, coords, _exact) = vh_value_and_coordinate_descriptors(ctx, vh);
            vh_access_mode_descriptor(access_type, &coords, &value_desc)
        }
    };
    varhandle_exact_mismatch(
        ctx,
        &vh_invoker_descriptor(&own)?,
        &vh_invoker_descriptor(desc)?,
    )
}

/// `MethodHandles.varHandleInvoker(mode, type)` (`exact == false`) and
/// `varHandleExactInvoker(mode, type)`: a handle of type `(VarHandle,
/// type...)R` that calls access mode `mode` on its first argument at
/// call-site type `type`, as HotSpot's `Invokers.varHandleMethodInvoker`
/// does; the exact one is marked [`MH_MARK_EXACT_INVOKER`] and refuses a
/// handle of another access mode type ([`vh_invoker_call_refusal`]).
///
/// The JDK's bodies bind a `LambdaForm` invoker (`BoundMethodHandle`), whose
/// species class did not link on this VM's `java.lang.invoke` model until
/// wave 51 (interpreter round i1 wave 50, lane L4; `L4W49VarHandleInvokers`).
/// Since wave 51 it links (`L4W51BoundMethodHandleSpecies`), and the JDK
/// body was measured in place of this one (lane L4): every row of
/// `L4W49VarHandleInvokers` and `L4W50VarHandleInvokerShapes` then answered
/// a default value (`0`, `null`, `false`), because the handle it returns is
/// a real `BoundMethodHandle$Species_L` over a `LambdaForm`, which this
/// model's invoke doors cannot run. So this native stays until real
/// `LambdaForm` handles are invokable here
/// (`i50-L5-bridges-still-run-over-jdk-bytecode-under-jdk-only`, the
/// `java.lang.invoke` rows). The handle here is the one
/// `VarHandle.toMethodHandle` binds (a direct handle's mode invoker).
pub fn mhs_var_handle_invoker(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    exact: bool,
) -> MethodCallResult {
    let (Some(Value::Object(Some(mode))), Some(Value::Object(Some(mt)))) =
        (args.first(), args.get(1))
    else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let (mode, mt) = (*mode, *mt);
    let mode_name = access_mode_name(ctx, mode);
    let method = access_mode_method_name(&mode_name);
    let desc = methodtype_to_descriptor(ctx, mt).unwrap_or_default();
    if method.is_empty() || desc.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("varHandleInvoker: unreadable access mode {mode_name} or type"),
        }
        .into());
    }
    vh_invoker_model_handle(ctx, &method, &desc, exact)
}

/// [`mhs_var_handle_invoker`]'s handle for the access-mode method `method`
/// (`get`, `compareAndSet`, ...) at call-site type `desc`.
pub fn vh_invoker_model_handle(
    ctx: &mut dyn NativeContext,
    method: &str,
    desc: &str,
    exact: bool,
) -> MethodCallResult {
    let mh = alloc_method_handle(ctx, VAR_HANDLE_CLASS, method, desc, MH_KIND_VIRTUAL)?;
    let mut bits = mh_marking_bits(&*ctx, mh) | MH_MARK_VH_INVOKER;
    if exact {
        bits |= MH_MARK_EXACT_INVOKER;
    }
    mh_write_marking_bits(&*ctx, mh, bits);
    Ok(Some(Value::Object(Some(mh))))
}

// ---------------------------------------------------------------------------
// VarHandle reference returns: `asType`'s checkcast (round 15 wave 2 hand-back,
// lane vh2; VH2-3,
// `r15w2-vh2-varhandle-reference-return-not-cast-to-site-type-FIXED-20260930.md`)
// ---------------------------------------------------------------------------

/// `CRATONVM_VH_REFERENCE_RETURN_CHECKCAST` (default on, both modes): a
/// `VarHandle` access that returns its variable into a call site declaring a
/// reference type other than `Object` is cast to it, as `asType` casts on
/// HotSpot ([`varhandle_reference_return_refusal`]); the JIT does not bind a
/// thin read helper it cannot make cast. `0` restores the boxes-only rule
/// (`vm_exec::varhandle_reference_return_mismatch`, WMTE).
pub fn vh_reference_return_checkcast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Whether `vh`'s variable has a reference type: `Some(true)` / `Some(false)`
/// for a handle that can describe its variable (an FFM layout handle's
/// carrier, a side-table row's field type, an `IndirectVarHandle`'s `value`,
/// a real array handle's component), `None` for any other. Read BEFORE the
/// access runs: the caller holds `vh` only as long as nothing moves it.
pub fn varhandle_variable_is_reference(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<bool> {
    let desc: String = if let Some(shape) = p67_segment_vh_shape(&*ctx, vh) {
        layout_vh_carrier_desc(shape.carrier).to_string()
    } else if let Some(meta) = vh_meta_get(ctx, vh) {
        meta.field_desc.clone()
    } else if is_indirect_var_handle(ctx, vh) {
        let value = indirect_vh_field(ctx, vh, "value")?;
        strict_mirror_descriptor(&*ctx, value)?
    } else {
        real_array_var_handle_descriptors(ctx, vh)?.0
    };
    match desc.as_bytes().first()? {
        b'L' | b'[' => Some(true),
        b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => Some(false),
        _ => None,
    }
}

/// The `ClassCastException` `asType` raises when a `VarHandle` access returns
/// `value` into a call site whose declared return is `site_ret` (a reference
/// descriptor other than `Ljava/lang/Object;`) and `value` is positively not
/// one, else `None`. HotSpot's cast is `Class.cast`'s, hence the wording
/// ([`reference_cast_failure`]).
///
/// * A class-typed site is judged by [`reference_arg_admitted`], which fails
///   OPEN (a stand-in, a proxy, a lambda, an unnameable class all pass).
/// * An array-typed site refuses a non-array, a primitive array of another
///   type and a reference array whose component `checkcast` refuses
///   ([`reference_array_target_admitted`], wave 60; "cannot tell" passes).
/// * A boxed primitive refused here is a CCE only when the variable is known
///   to be a reference (`variable_is_reference`, from
///   [`varhandle_variable_is_reference`]). For a primitive variable `asType`
///   refuses the conversion statically, a `WrongMethodTypeException`, which
///   `vm_exec::unbox_poly_return_checked`'s W6-1 rule raises; an undescribed
///   variable keeps that rule too.
pub fn varhandle_reference_return_refusal(
    ctx: &mut dyn NativeContext,
    value: Value,
    site_ret: &str,
    variable_is_reference: Option<bool>,
) -> Option<MethodCallFailed> {
    if !vh_reference_return_checkcast_enabled() {
        return None;
    }
    let Value::Object(Some(obj)) = value else {
        return None;
    };
    if site_ret.starts_with('[') {
        // `checkcast`'s array rule, component included (wave 60, lane L4).
        if reference_array_target_admitted(&*ctx, obj, site_ret) != Some(false) {
            return None;
        }
        return Some(reference_cast_failure(ctx, value, site_ret));
    }
    if reference_arg_admitted(ctx, value, site_ret) != Some(false) {
        return None;
    }
    let is_box = !ctx.object_is_array(obj)
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(obj))
            .is_some_and(|name| {
                matches!(
                    &*name,
                    "java/lang/Integer"
                        | "java/lang/Long"
                        | "java/lang/Short"
                        | "java/lang/Byte"
                        | "java/lang/Character"
                        | "java/lang/Boolean"
                        | "java/lang/Float"
                        | "java/lang/Double"
                )
            });
    if is_box && variable_is_reference != Some(true) {
        return None;
    }
    Some(reference_cast_failure(ctx, value, site_ret))
}


pub fn vh_value_and_coordinate_descriptors(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
) -> (i32, String, Vec<String>, bool) {
    // An FFM layout handle describes itself out of the shape table — the
    // carrier the layout named and `(MemorySegment, long[, long])` — and must
    // be answered before the generic paths, whose slot reads would take this
    // receiver's endianness flag for a kind tag (that is the `kind 0` in the
    // refusal `RJdkForeign.layoutVarHandles` used to raise).
    if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
        return (
            VH_KIND_MEMORY_SEGMENT_LAYOUT,
            layout_vh_carrier_desc(shape.carrier).to_string(),
            layout_vh_coordinates(shape),
            true,
        );
    }
    // Compatible mode can leave a concrete JDK `SegmentVarHandle` unwrapped
    // by our side table. Its VarForm is the authoritative carrier/coordinate
    // description, just as the registered shape is for handles CratonVM mints.
    if let Some((value_desc, coords)) = real_segment_vh_descriptors(ctx, vh) {
        return (VH_KIND_MEMORY_SEGMENT_LAYOUT, value_desc, coords, true);
    }
    let meta = vh_meta_get(ctx, vh);
    // A real-JDK array handle has no side-table entry and its slots belong to
    // the real class, so the fallbacks below would read `vform` as a kind tag.
    // Ask the object what it is instead. Checked only when the side table has
    // nothing: a handle CratonVM minted is described by the metadata CratonVM
    // recorded for it.
    if meta.is_none() {
        if let Some((value_desc, coords)) = real_array_var_handle_descriptors(ctx, vh) {
            return (VH_KIND_ARRAY, value_desc, coords, true);
        }
    }
    let kind = meta
        .as_deref()
        .map(|m| m.kind)
        .or_else(|| ctx.get_field(vh, VH_KIND).as_int())
        .unwrap_or(VH_KIND_INSTANCE);
    let value_desc = meta
        .as_deref()
        .map(|m| Cow::Owned(m.field_desc.clone()))
        .unwrap_or_else(|| vh_field_desc(ctx, vh));
    let mut coords_exact = true;
    let coords = match kind {
        VH_KIND_STATIC => Vec::new(),
        VH_KIND_ARRAY => {
            // `class_name` is the ARRAY class's internal name (`[I`,
            // `[Ljava/lang/String;`), written by this file's
            // `arrayElementVarHandle`. Absent it, the leading coordinate would
            // be an erasure rather than the `int[]`/`String[]` the JDK
            // reports, and `coords_exact` goes false so `coordinateTypes()`
            // refuses instead of naming `Object`.
            let arr = meta
                .as_deref()
                .map(|m| m.class_name.as_str())
                .filter(|n| !n.is_empty())
                .map(|n| class_name_to_descriptor(n).into_owned());
            match arr {
                Some(a) => vec![a, DESC_INT.to_string()],
                None => {
                    coords_exact = false;
                    vec![DESC_OBJECT.to_string(), DESC_INT.to_string()]
                }
            }
        }
        VH_KIND_BYTE_VIEW_LE | VH_KIND_BYTE_VIEW_BE => {
            vec!["[B".to_string(), DESC_INT.to_string()]
        }
        VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE => {
            vec!["Ljava/nio/ByteBuffer;".to_string(), DESC_INT.to_string()]
        }
        _ => {
            let receiver = meta
                .as_deref()
                .map(|m| m.class_name.as_str())
                .filter(|n| !n.is_empty())
                .map(|n| class_name_to_descriptor(n).into_owned());
            match receiver {
                Some(r) => vec![r],
                None => {
                    coords_exact = false;
                    vec![DESC_OBJECT.to_string()]
                }
            }
        }
    };
    (kind, value_desc.into_owned(), coords, coords_exact)
}

pub fn varhandle_access_mode_type_uncached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let access_type = match args.get(1) {
        Some(Value::Object(Some(access_type))) => *access_type,
        _ => return Ok(None),
    };
    let access_type = vh_access_type_ordinal(ctx, access_type);

    let desc = if let Some(desc) = p67_memory_segment_varhandle_descriptor(ctx, this, access_type) {
        desc
    } else {
        let (_kind, value_desc, coords, _exact) = vh_value_and_coordinate_descriptors(ctx, this);
        vh_access_mode_descriptor(access_type, &coords, &value_desc)
    };

    match build_method_type_from_descriptor(ctx, &desc)? {
        Some(mt) => Ok(Some(Value::Object(Some(mt)))),
        None => Ok(None),
    }
}

/// The `Class` mirror for a JVM field descriptor — `"I"` → `int.class`,
/// `"Ljava/lang/String;"` → `String.class`.
///
/// Both lookups are canonical caches (`primitive_mirrors` in `vm_object.rs`,
/// `get_or_create_class_mirror` for reference types), which is what makes
/// `vi.varType() == int.class` an IDENTITY match: the mirror the vector gets
/// from `getstatic Integer.TYPE` and the one produced here are the same object.
/// Same idiom as `build_method_type_from_descriptor` above.
pub fn vh_descriptor_mirror(ctx: &mut dyn NativeContext, desc: &str) -> ObjectRef {
    // An ARRAY descriptor has to go through `lang_class`, which knows to ask
    // the class manager to intern `[I` and falls back to a synthetic mirror.
    // The generic path below would end at `primitive_class_mirror("[I")` —
    // a fabricated PRIMITIVE mirror named `[I`, which is not `int[].class`
    // and would not compare equal to it. This matters for the array-element
    // handle's leading coordinate (`coordinateTypes()[0]`), which HotSpot
    // reports as the array class itself (measured: `[class [I, int]`).
    if desc.starts_with('[') {
        return crate::lang_class::descriptor_to_class_mirror(ctx, desc);
    }
    let name = descriptor_to_class_name(desc);
    match ctx.class_id_by_name(&name) {
        Some(cid) => ctx.get_class_mirror(cid),
        None => ctx.primitive_class_mirror(&name),
    }
}

/// The refusal both describe-yourself accessors raise when the receiver's
/// metadata cannot name what it addresses.
///
/// There is no JDK failure mode here — every real VarHandle can describe
/// itself — so any exception is a CratonVM deviation. It is still the right
/// one: the alternative is inventing a `Class`, and a fabricated variable type
/// is exactly the "plausible value where the VM does not know" shape the
/// strict corpus exists to find. `UnsupportedOperationException` is catchable
/// and cannot be mistaken for an answer.
pub fn vh_undescribable(what: &str, kind: i32) -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: format!(
            "VarHandle.{what}: this CratonVM VarHandle carries no variable/coordinate type \
             metadata (kind {kind}); it was minted by a factory that does not record it, so \
             the answer is unknown rather than absent"
        ),
    }
    .into()
}

/// Build the `List<Class<?>>` that `coordinateTypes()` returns.
///
/// `List.of(Object[])` is the JDK-owned immutable-list construction, so the
/// result `equals` the `List.of(...)` a caller compares it against
/// (`RJdkHandles:245` asserts exactly that). `Arrays.asList` is the
/// synthetic-JDK fallback, matching `lang_system.rs`'s `boxed_int_list`.
///
/// Every mirror is pinned across the allocations: `new_array` and the `List.of`
/// invocation can both collect, and a raw `ObjectRef` held over a collection is
/// stale.
pub fn vh_class_list(ctx: &mut dyn NativeContext, descs: &[String]) -> Option<Value> {
    let mut mirrors: Vec<ObjectRef> = Vec::with_capacity(descs.len());
    let mut pins: Vec<usize> = Vec::with_capacity(descs.len());
    let mut base: Option<usize> = None;
    for d in descs {
        // Re-read every earlier mirror: resolving this one can allocate.
        for (i, h) in pins.iter().enumerate() {
            mirrors[i] = ctx.read_native_pin(*h, mirrors[i]);
        }
        let m = vh_descriptor_mirror(ctx, d);
        let h = ctx.pin_native_root(m);
        if base.is_none() {
            base = Some(h);
        }
        mirrors.push(m);
        pins.push(h);
    }
    let array = ctx.new_array(ArrayElementType::Reference, descs.len());
    let array_pin = ctx.pin_native_root(array);
    if base.is_none() {
        base = Some(array_pin);
    }
    let mut array = array;
    for (i, h) in pins.iter().enumerate() {
        let m = ctx.read_native_pin(*h, mirrors[i]);
        array = ctx.read_native_pin(array_pin, array);
        ctx.set_array_element(array, i, Value::Object(Some(m)));
    }
    array = ctx.read_native_pin(array_pin, array);
    let list = ctx
        .invoke(
            "java/util/List",
            "of",
            "([Ljava/lang/Object;)Ljava/util/List;",
            &[Value::Object(Some(array))],
        )
        .ok()
        .flatten();
    let list = match list {
        Some(Value::Object(Some(_))) => list,
        _ => {
            let array = ctx.read_native_pin(array_pin, array);
            ctx.invoke(
                "java/util/Arrays",
                "asList",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(array))],
            )
            .ok()
            .flatten()
        }
    };
    if let Some(b) = base {
        ctx.unpin_native_roots(b);
    }
    match list {
        Some(Value::Object(Some(_))) => list,
        _ => None,
    }
}

/// Is this handle's metadata authoritative enough to describe itself?
///
/// The WP4.2 side table is "the canonical truth" (see its banner) — every
/// factory in this file writes one. Without an entry, `vh_field_desc` falls
/// back to `DESC_OBJECT`, which is a workable erasure for `accessModeType` but
/// would make `varType()` report `Object` for, say, a real-JDK `int[]` handle
/// that reached us without going through our factories. Reporting a fabricated
/// `Class` is the one outcome these accessors must not produce.
///
/// A real SegmentVarHandle and a REAL-JDK array-element VarHandle are also
/// documented sources of truth: neither has a side-table entry, but each
/// names its own value and coordinate types through its JDK form
/// (see [`real_segment_vh_descriptors`] and
/// [`real_array_var_handle_descriptors`]).
pub fn vh_meta_is_authoritative(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    // The FFM layout table is a third source of truth, and a recorded one: the
    // carrier comes from the layout the handle was minted from, not from a
    // guess about the receiver's slots.
    p67_segment_vh_shape(ctx, vh).is_some()
        || vh_meta_get(ctx, vh).is_some()
        || real_segment_vh_descriptors(ctx, vh).is_some()
        || real_array_var_handle_descriptors(ctx, vh).is_some()
}

pub fn varhandle_var_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // A combinator's handle names its own type (round 14 wave 7).
    if let Some(value) = indirect_vh_var_type(ctx, this) {
        return Ok(Some(Value::Object(Some(value))));
    }
    let (kind, value_desc, _coords, _exact) = vh_value_and_coordinate_descriptors(ctx, this);
    // An empty or non-authoritative descriptor is "we never recorded one",
    // not a type.
    if value_desc.is_empty() || !vh_meta_is_authoritative(ctx, this) {
        return Err(vh_undescribable("varType", kind));
    }
    let mirror = vh_descriptor_mirror(ctx, &value_desc);
    Ok(Some(Value::Object(Some(mirror))))
}

pub fn varhandle_coordinate_types(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(list) = indirect_vh_coordinate_types(ctx, this) {
        return Ok(Some(list));
    }
    if let Some(list) = vh_lambda_proxy_coordinate_types(ctx, this) {
        return Ok(Some(list));
    }
    let (kind, _value_desc, coords, exact) = vh_value_and_coordinate_descriptors(ctx, this);
    if !exact || !vh_meta_is_authoritative(ctx, this) {
        return Err(vh_undescribable("coordinateTypes", kind));
    }
    match vh_class_list(ctx, &coords) {
        Some(v) => Ok(Some(v)),
        // Returning null here would be a null `coordinateTypes()`, which no
        // real VarHandle ever answers.
        None => Err(vh_undescribable("coordinateTypes", kind)),
    }
}

/// `coordinateTypes()` of an instance `VarHandle` over a lambda proxy's
/// capture (interpreter round i3 wave 79, lane L4; [`lk_lambda_proxy_var_handle`]):
/// `List.of(P.class)`, the proxy's own mirror by its recorded `ClassId`. The
/// descriptor path ([`vh_class_list`]) resolves the coordinate by NAME, which
/// a proxy (outside the class store) does not have. `None` for every other
/// handle (one meta read and an id compare). The array is a `Class[]`, which
/// `List.of(Object...)` takes as is.
pub fn vh_lambda_proxy_coordinate_types(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<Value> {
    let meta = vh_meta_get(ctx, vh)?;
    if meta.kind != VH_KIND_INSTANCE || meta.class_id < 0x8000_0000 {
        return None;
    }
    let proxy = ClassId::new(meta.class_id);
    crate::lang_class::lambda_proxy_class_name(&*ctx, proxy)?;
    let mirror = ctx.get_class_mirror(proxy);
    let mirror_pin = ctx.pin_native_root(mirror);
    let array = new_class_array(ctx, 1);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    ctx.set_array_element(array, 0, Value::Object(Some(mirror)));
    let array_pin = ctx.pin_native_root(array);
    let list = ctx
        .invoke(
            "java/util/List",
            "of",
            "([Ljava/lang/Object;)Ljava/util/List;",
            &[Value::Object(Some(array))],
        )
        .ok()
        .flatten();
    let list = match list {
        Some(Value::Object(Some(_))) => list,
        _ => {
            let array = ctx.read_native_pin(array_pin, array);
            ctx.invoke(
                "java/util/Arrays",
                "asList",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(array))],
            )
            .ok()
            .flatten()
        }
    };
    ctx.unpin_native_roots(mirror_pin);
    match list {
        Some(Value::Object(Some(_))) => list,
        _ => None,
    }
}

/// Read `width` bytes of a `byte[]` at BYTE index `idx`, assembled per
/// endianness, and box them as the view element type. This is the correct
/// `byteArrayViewVarHandle` get — distinct from an array-element access (which
/// would return a single signed byte). CratonVM stores `byte[]` elements as
/// signed `Value::Int`.
/// Bounds-check a byte-array-view `VarHandle` access, JDK-style.
///
/// `MethodHandles.byteArrayViewVarHandle` checks the START index against
/// `array.length - (width - 1)` — the number of positions a `width`-byte
/// element can start at — and reports it that way:
/// `Index -1 out of bounds for length 15` for a `short` view over a 16-byte
/// array, not "for length 16". `RuntimeError::aioobe` formats the JDK's
/// `Preconditions.checkIndex` wording, so the message matches character for
/// character.
///
/// There was no check at all here. Both `get` and `set` took the raw `int`
/// coordinate as `*i as usize`, so a negative index wrapped to an enormous
/// `usize` and the per-byte loop below simply read zeros or dropped the writes
/// — an out-of-bounds **write** that reported success. Nothing in the corpus
/// caught it because netty only takes this path when `sun.misc.Unsafe` is
/// unavailable, and CratonVM pinned `sun.misc.unsafe.memory.access` so netty
/// never did; on stock HotSpot 25, which does not pin it, netty's heap
/// `ByteBuf` reads and writes go through exactly this VarHandle.
pub fn byte_view_check_index(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    index: i32,
    elem: u8,
) -> Result<usize, MethodCallFailed> {
    let width = i64::from(byte_view_width(elem) as u32);
    let length = ctx.array_length(arr) as i64;
    // The JDK passes this signed bound to Preconditions.checkIndex, including
    // a negative length when the carrier is shorter than one view element.
    let limit = length - (width - 1);
    if i64::from(index) < 0 || i64::from(index) >= limit {
        return Err(RuntimeError::aioobe(index, limit as i32).into());
    }
    Ok(index as usize)
}

pub fn byte_view_get(ctx: &dyn NativeContext, arr: ObjectRef, idx: usize, elem: u8, le: bool) -> Value {
    let w = byte_view_width(elem);
    let mut raw: u64 = 0;
    for i in 0..w {
        let b = ctx.get_array_element(arr, idx + i).as_int().unwrap_or(0) as u8 as u64;
        if le {
            raw |= b << (8 * i);
        } else {
            raw = (raw << 8) | b;
        }
    }
    match elem {
        b'J' => Value::Long(raw as i64),
        b'D' => Value::Double(f64::from_bits(raw)),
        b'I' => Value::Int(raw as u32 as i32),
        b'F' => Value::Float(f32::from_bits(raw as u32)),
        b'S' => Value::Int((raw as u16) as i16 as i32), // short, sign-extended
        b'C' => Value::Int((raw as u16) as i32),        // char, zero-extended
        _ => Value::Int(raw as u8 as i8 as i32),
    }
}

/// Decompose `value` into `width` bytes per endianness and write them into the
/// `byte[]` at BYTE index `idx`. The correct `byteArrayViewVarHandle` set.
pub fn byte_view_set(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    elem: u8,
    le: bool,
    value: &Value,
) {
    let w = byte_view_width(elem);
    let raw: u64 = match value {
        Value::Long(v) => *v as u64,
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => *v as u32 as u64,
        Value::Float(v) => v.to_bits() as u64,
        _ => 0,
    };
    for i in 0..w {
        let shift = if le { 8 * i } else { 8 * (w - 1 - i) };
        let b = ((raw >> shift) & 0xff) as u8 as i8 as i32;
        ctx.set_array_element(arr, idx + i, Value::Int(b));
    }
}

/// If `meta` is a byte-array-view VarHandle, return `(element_desc, little_endian)`.
pub fn byte_view_kind(meta: Option<&VarHandleMeta>) -> Option<(u8, bool)> {
    let m = meta?;
    let le = match m.kind {
        VH_KIND_BYTE_VIEW_LE => true,
        VH_KIND_BYTE_VIEW_BE => false,
        _ => return None,
    };
    Some((*m.field_desc.as_bytes().first().unwrap_or(&b'J'), le))}

/// If `meta` is a byte-buffer-view VarHandle, return `(element_desc, little_endian)`.
pub fn byte_buffer_view_kind(meta: Option<&VarHandleMeta>) -> Option<(u8, bool)> {
    let m = meta?;
    let le = match m.kind {
        VH_KIND_BYTE_BUFFER_VIEW_LE => true,
        VH_KIND_BYTE_BUFFER_VIEW_BE => false,
        _ => return None,
    };
    Some((*m.field_desc.as_bytes().first().unwrap_or(&b'J'), le))}

pub fn byte_view_decode(elem: u8, le: bool, bytes: &[u8]) -> Value {
    let mut raw: u64 = 0;
    if le {
        for (i, &b) in bytes.iter().enumerate() {
            raw |= (b as u64) << (8 * i);
        }
    } else {
        for &b in bytes {
            raw = (raw << 8) | b as u64;
        }
    }
    match elem {
        b'J' => Value::Long(raw as i64),
        b'D' => Value::Double(f64::from_bits(raw)),
        b'I' => Value::Int(raw as u32 as i32),
        b'F' => Value::Float(f32::from_bits(raw as u32)),
        b'S' => Value::Int((raw as u16) as i16 as i32),
        b'C' => Value::Int((raw as u16) as i32),
        _ => Value::Int(raw as u8 as i8 as i32),
    }
}

pub fn byte_view_encode(elem: u8, le: bool, value: &Value) -> [u8; 8] {
    let w = byte_view_width(elem);
    let raw: u64 = match value {
        Value::Long(v) => *v as u64,
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => *v as u32 as u64,
        Value::Float(v) => v.to_bits() as u64,
        _ => 0,
    };
    let mut bytes = [0u8; 8];
    for i in 0..w {
        let shift = if le { 8 * i } else { 8 * (w - 1 - i) };
        bytes[i] = ((raw >> shift) & 0xff) as u8;
    }
    bytes
}

/// Bounds-check a byte-BUFFER-view `VarHandle` access.
///
/// The sibling of [`byte_view_check_index`]. HotSpot's contract differs in the
/// exception TYPE: a buffer view raises plain `IndexOutOfBoundsException` where
/// an array view raises `ArrayIndexOutOfBoundsException`. Both use
/// `Preconditions.checkIndex`'s wording and both bound the START index by
/// `length - (width - 1)` — for a buffer that length is its **limit**.
/// Measured on JDK 25: `IndexOutOfBoundsException: Index -1 out of bounds for
/// length 15` for a `short` view over a 16-byte buffer.
///
/// There was no check. Both call sites clamped a negative coordinate to `0`
/// instead, so `get(buf, -1)` silently read element 0 and `set(buf, -1, v)`
/// silently overwrote it — a wrong answer where the JDK throws, and on the
/// `set` side a write to memory the caller never named.
pub fn byte_buffer_view_check_index(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    index: i32,
    elem: u8,
) -> Result<usize, MethodCallFailed> {
    let width = i64::from(byte_view_width(elem) as u32);
    let limit = match ctx.get_field_by_name(bb, "limit") {
        Value::Int(l) => i64::from(l),
        _ => 0,
    };
    let bound = limit - (width - 1);
    if i64::from(index) < 0 || i64::from(index) >= bound {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(cratonvm_types::error::out_of_bounds_message::check_index(
                i64::from(index),
                bound,
            )),
        }
        .into());
    }
    Ok(index as usize)}

/// Is this `ByteBuffer` a read-only view?
///
/// `VarHandle.set` through a byte-buffer view must raise
/// `ReadOnlyBufferException`; CratonVM performed the write instead, so a
/// `asReadOnlyBuffer()` handed to code that mutates it was not read-only at
/// all. Two signals, because the field is not present on every buffer shape
/// this VM can produce: the `isReadOnly` field when there is one, and
/// otherwise the JDK's own naming — the read-only views are exactly
/// `HeapByteBufferR` and `DirectByteBufferR`.
pub fn byte_buffer_view_is_read_only(ctx: &mut dyn NativeContext, bb: ObjectRef) -> bool {
    if let Value::Int(flag) = ctx.get_field_by_name(bb, "isReadOnly") {
        if flag != 0 {
            return true;
        }
    }
    ctx.class_name_of_id(ctx.class_id_of_object(bb))
        .map_or(false, |name| name.ends_with("ByteBufferR"))
}

/// The native address of a **direct** buffer's byte `idx`, or `None`.
///
/// The `hb == null` test is the whole point. `java.nio.Buffer.address` is NOT
/// zero for a heap buffer on JDK 21+ — it holds the array base offset (16 on
/// this platform) so that `Unsafe` accesses can use one code path for both
/// kinds. Reading it as an absolute pointer therefore dereferenced address
/// `16 + idx` for every heap buffer, which is a hard SIGSEGV, and the
/// heap-array fallback below it was unreachable:
///
/// ```text
/// SIGSEGV at pc=…, addr=0x10        # 0x10 == 16
/// ```
///
/// `hb` (the backing `byte[]`) is the field that actually distinguishes the
/// two: null on a direct buffer, non-null on a heap one.
pub fn byte_buffer_view_addr(ctx: &mut dyn NativeContext, bb: ObjectRef, idx: usize) -> Option<i64> {
    if matches!(ctx.get_field_by_name(bb, "hb"), Value::Object(Some(_))) {
        return None;
    }
    let base = match ctx.get_field_by_name(bb, "address") {
        Value::Long(a) if a > 0 => a,
        _ => return None,
    };
    base.checked_add(idx as i64)}

pub fn byte_buffer_view_heap_array(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    idx: usize,
) -> Option<(ObjectRef, usize)> {
    let arr = match ctx.get_field_by_name(bb, "hb") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let off = match ctx.get_field_by_name(bb, "offset") {
        Value::Int(i) if i >= 0 => i as usize,
        _ => 0,
    };
    Some((arr, off + idx))}

/// The stored session field of an understood buffer segment. This uses the
/// existing real-segment layout resolver or the native carrier's own resolver;
/// unknown shapes stay on the authoritative checked access.
pub fn byte_buffer_view_stored_session(
    ctx: &dyn NativeContext,
    segment: ObjectRef,
) -> Option<(ObjectRef, usize)> {
    if let Some(stored) = crate::panama::jit_carrier_stored_session(ctx, segment) {
        return Some(stored);
    }
    let fields = seg_lean_class(ctx, ctx.class_id_of_object(segment))?;
    match ctx.get_field(segment, fields.scope) {
        Value::Object(Some(session)) => Some((session, fields.scope)),
        _ => None,
    }
}

/// Hold shared-close protection from the scope check through the access.
/// Bounds, read-only and alignment refusals have already run. The scope check
/// can invoke Java on an unfamiliar or failing session, so pin the buffer and
/// hand its forwarded reference back before reading any payload fields.
pub fn byte_buffer_view_access_window(
    ctx: &mut dyn NativeContext,
    bb: &mut ObjectRef,
) -> Result<crate::ffm_fast::AccessWindow, MethodCallFailed> {
    let window = crate::ffm_fast::AccessWindow::open();
    let epoch = crate::ffm_fast::epoch();
    if let Value::Object(Some(mut segment)) = ctx.get_field_by_name(*bb, "segment") {
        let pin = ctx.pin_native_root(*bb);
        let checked = crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, &mut segment);
        *bb = ctx.read_native_pin(pin, *bb);
        ctx.unpin_native_roots(pin);
        checked?;
        if let Some((session, _)) = byte_buffer_view_stored_session(ctx, segment) {
            crate::phases_late::foreign_ffm::p67_note_checked_session(ctx, session, epoch);
        }
    }
    Ok(window)
}

pub fn byte_buffer_view_get(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    idx: usize,
    elem: u8,
    le: bool,
) -> Option<Value> {
    let w = byte_view_width(elem);
    if let Some(addr) = byte_buffer_view_addr(ctx, bb, idx) {
        let mut bytes = [0u8; 8];
        if ctx.copy_from_native_memory(addr, &mut bytes[..w]) {
            return Some(byte_view_decode(elem, le, &bytes[..w]));
        }
    }
    if let Some((arr, start)) = byte_buffer_view_heap_array(ctx, bb, idx) {
        return Some(byte_view_get(ctx, arr, start, elem, le));
    }
    None
}

pub fn byte_buffer_view_set(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    idx: usize,
    elem: u8,
    le: bool,
    value: &Value,
) {
    let w = byte_view_width(elem);
    let bytes = byte_view_encode(elem, le, value);
    if let Some(addr) = byte_buffer_view_addr(ctx, bb, idx) {
        if ctx.copy_to_native_memory(addr, &bytes[..w]) {
            return;
        }
    }
    if let Some((arr, start)) = byte_buffer_view_heap_array(ctx, bb, idx) {
        for (i, &b) in bytes[..w].iter().enumerate() {
            ctx.set_array_element(arr, start + i, Value::Int(b as i8 as i32));
        }
    }
}

// ---------------------------------------------------------------------------
// java.lang.foreign / SegmentVarHandle (JEP 454 FFM API) coordinate access
// ---------------------------------------------------------------------------
//
// `MemorySegment.get/set(ValueLayout, long)` real bytecode
// (`jdk.internal.foreign.AbstractMemorySegmentImpl`) calls straight into
// `SegmentVarHandle.get/set(segment, offset[, value])`. That class is real
// bytecode with no native body for these signature-polymorphic accessors (no
// concrete `get`/`set` method exists on it at all — see JVMS §5.4.3.3), so it
// has no entry in our synthetic `VH_KIND` side-table either. Detected purely
// by the VarHandle's own (real) class name, distinct from the array-element
// and byte-array-view shapes handled above.

pub const SEGMENT_VAR_HANDLE_CLASS: &str = "java/lang/invoke/SegmentVarHandle";

pub fn is_segment_var_handle(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        == Some(SEGMENT_VAR_HANDLE_CLASS)
}

/// A `SegmentVarHandle`'s own `enclosing` (the `ValueLayout` it was built
/// from), baked-in `offset`, and `be` (byte order requires a swap) fields —
/// resolved by name since this is a real class (real, JDK-image field
/// layout), not our synthetic 6-field VarHandle shape.
pub fn segment_vh_fields(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(ObjectRef, i64, bool)> {
    let enclosing_idx = ctx.resolve_field_index(SEGMENT_VAR_HANDLE_CLASS, "enclosing")?;
    let offset_idx = ctx.resolve_field_index(SEGMENT_VAR_HANDLE_CLASS, "offset")?;
    let be_idx = ctx.resolve_field_index(SEGMENT_VAR_HANDLE_CLASS, "be")?;
    let enclosing = match ctx.get_field(vh, enclosing_idx) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let offset = match ctx.get_field(vh, offset_idx) {
        Value::Long(n) => n,
        _ => 0,
    };
    let be = matches!(ctx.get_field(vh, be_idx), Value::Int(n) if n != 0);
    Some((enclosing, offset, be))
}

/// SBR3 lane ffm: everything a real `SegmentVarHandle` access needs from the
/// HANDLE, decoded once. The handle is immutable (its layout, offset, byte
/// order and `VarForm` are final), but reading them cost three field-index
/// resolutions by name, a `VarForm` walk with a class-name render, four reads
/// for the variable-offset test and two layout queries on EVERY access --
/// about a third of a `get`/`set`'s time once the layout `varHandle`s run the
/// JDK's own factories.
#[derive(Clone, Copy)]
pub struct SegVhPlan {
    pub shape: SegShape,
    pub enclosing_size: i64,
    pub enclosing_align: i64,
    pub vh_offset: i64,
    pub be: bool,
    pub variable: bool,
    /// JIT round 20 wave 8 (lane huntFFM): the handle's `VarForm` is an
    /// ATOMIC-capable one -- `VarHandleSegmentAs<X>Aligned`, or the bytes /
    /// booleans form, which has no unaligned twin
    /// ([`segment_vh_form_class_is_aligned`]). JDK 25 picks it from the
    /// accessed VALUE layout's alignment (`Utils.makeRawSegmentViewVarHandle`
    /// passes `layout.byteAlignment() - 1`), not from `enclosing`, which is the
    /// GROUP layout of a path handle: a struct holding a `JAVA_LONG` and a
    /// `JAVA_INT_UNALIGNED` is 8-aligned, and its `x` handle still has only
    /// `get` / `set`.
    pub aligned_form: bool,
}

thread_local! {
    /// Per-thread memo of [`seg_vh_plan`], on the terms of [`P67_SHAPE_MEMO`]
    /// (keyed by VM, identity hash, address and [`VH_META_GENERATION`]).
    pub static SEG_VH_PLAN_MEMO: std::cell::RefCell<Vec<VhMemoLine<SegVhPlan>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// The [`SegVhPlan`] of a real `SegmentVarHandle`, or `None` when its form or
/// layout cannot be read (the caller keeps the unplanned reading).
pub fn seg_vh_plan(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<SegVhPlan> {
    let hit = vh_memo_lookup(&SEG_VH_PLAN_MEMO, &*ctx, vh, |_| None);
    if hit.is_some() {
        return hit;
    }
    let (enclosing, vh_offset, be) = segment_vh_fields(ctx, vh)?;
    let (shape, aligned_form) = segment_vh_form_info(ctx, vh)?;
    let variable = segment_vh_form_is_variable(ctx, vh);
    let enclosing_size = match crate::panama_libffi::layout_total_size(ctx, enclosing) {
        Ok(n) => n as i64,
        Err(_) => return None,
    };
    let enclosing_align = crate::panama_libffi::layout_align(ctx, enclosing) as i64;
    let plan = SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        variable,
        aligned_form,
    };
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(vh);
    let addr = vh.as_ptr() as usize;
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    SEG_VH_PLAN_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = VhMemoLine {
            vm,
            key: hash,
            addr,
            generation,
            value: Some(plan),
        };
    });
    Some(plan)
}

/// Primitive "shape" of a real `ValueLayout`, read off its implementation
/// class name (`jdk.internal.foreign.layout.ValueLayouts$OfXxxImpl`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SegShape {
    Byte,
    Boolean,
    Short,
    Char,
    Int,
    Long,
    Float,
    Double,
    Address,
}

pub fn segment_layout_shape(ctx: &mut dyn NativeContext, layout: ObjectRef) -> SegShape {
    let name = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    if name.contains("OfByte") {
        SegShape::Byte
    } else if name.contains("OfBoolean") {
        SegShape::Boolean
    } else if name.contains("OfShort") {
        SegShape::Short
    } else if name.contains("OfChar") {
        SegShape::Char
    } else if name.contains("OfInt") {
        SegShape::Int
    } else if name.contains("OfLong") {
        SegShape::Long
    } else if name.contains("OfFloat") {
        SegShape::Float
    } else if name.contains("OfDouble") {
        SegShape::Double
    } else {
        SegShape::Address
    }
}

pub fn seg_shape_width(shape: SegShape) -> i64 {
    match shape {
        SegShape::Byte | SegShape::Boolean => 1,
        SegShape::Short | SegShape::Char => 2,
        SegShape::Int | SegShape::Float => 4,
        SegShape::Long | SegShape::Double | SegShape::Address => 8,
    }
}

pub fn seg_swap_bytes(raw: u64, width: i64) -> u64 {
    match width {
        1 => raw,
        2 => (raw as u16).swap_bytes() as u64,
        4 => (raw as u32).swap_bytes() as u64,
        _ => raw.swap_bytes(),
    }
}

pub fn seg_decode_value(shape: SegShape, raw: u64) -> Value {
    match shape {
        SegShape::Byte => Value::Int(raw as u8 as i8 as i32),
        SegShape::Boolean => Value::Int((raw as u8 != 0) as i32),
        SegShape::Short => Value::Int(raw as u16 as i16 as i32),
        SegShape::Char => Value::Int(raw as u16 as i32),
        SegShape::Int => Value::Int(raw as u32 as i32),
        SegShape::Float => Value::Float(f32::from_bits(raw as u32)),
        SegShape::Long | SegShape::Address => Value::Long(raw as i64),
        SegShape::Double => Value::Double(f64::from_bits(raw)),
    }
}

pub fn seg_shape_desc(shape: SegShape) -> &'static str {
    match shape {
        SegShape::Byte => "B",
        SegShape::Boolean => "Z",
        SegShape::Short => "S",
        SegShape::Char => "C",
        SegShape::Int => "I",
        SegShape::Long => "J",
        SegShape::Float => "F",
        SegShape::Double => "D",
        SegShape::Address => "J",
    }
}

/// `CRATONVM_FFM_SEGMENT_VH_FORM` (default on; round 13 wave 1, lane mhffm,
/// both modes): a real `SegmentVarHandle` is read as JDK 25 builds it -- the
/// value type from its `VarForm` (`VarHandles.memorySegmentViewHandle` picks
/// `VarHandleSegmentAs<Carrier>s` from the BASE carrier: `long` for an
/// `ADDRESS` layout, whose `MemorySegment` view is a `filterValue` adapter
/// above this handle), `enclosing` as the layout the coordinate must fit and
/// be aligned for (`checkEnclosingLayout`: a path handle's `enclosing` is the
/// GROUP layout), the variable-offset form's extra `long` coordinate, and the
/// JDK's check order (read-only, bounds, alignment, then the session). `0`
/// restores round 12 wave 7's reading (value type and bounds from
/// `enclosing`, the scope checked first).
pub fn segment_vh_form_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// A field of a real JDK object: by name on the object, else through the
/// declaring class's field table (`get_field_by_name` answers null for a name
/// it cannot resolve, as `panama::heap_seg_field` notes).
pub fn real_object_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    declaring: &str,
    name: &str,
) -> Value {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(None) => match ctx.resolve_field_index(declaring, name) {
            Some(index) => ctx.get_field(obj, index),
            None => Value::Object(None),
        },
        other => other,
    }
}

/// The value shape of a real `SegmentVarHandle`, from its `VarForm`'s
/// `implClass` (`java/lang/invoke/VarHandleSegmentAsInts`, or its
/// `...Aligned` subclass). `None` when the form cannot be read, which keeps
/// the `enclosing`-derived reading.
pub fn segment_vh_form_shape(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<SegShape> {
    segment_vh_form_info(ctx, vh).map(|(shape, _)| shape)
}

/// [`segment_vh_form_shape`] plus whether the form is atomic-capable
/// ([`segment_vh_form_class_is_aligned`]), read off the same `implClass`.
pub fn segment_vh_form_info(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(SegShape, bool)> {
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return None;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "implClass")
    else {
        return None;
    };
    let impl_class = ctx.class_id_from_mirror(impl_mirror)?;
    let name = ctx.class_name_of_id(impl_class)?;
    let shape = segment_vh_shape_of_form_class(&name)?;
    Some((shape, segment_vh_form_class_is_aligned(&name)))
}

/// JIT round 20 wave 8 (lane huntFFM): does the form class `name` (one
/// [`segment_vh_shape_of_form_class`] accepts) define the access modes past
/// `get` / `set`? JDK 25's `VarHandleSegmentAs<X>.selectForm` hands an
/// UNALIGNED value layout (`alignmentMask` short of the carrier's size) the
/// base class, which declares `get` / `set` only, and an aligned one the
/// `...Aligned` subclass, which adds the ordered and read-modify-write modes;
/// `VarHandleSegmentAsBytes` / `...Booleans` have no subclass and declare the
/// ordered modes themselves. Measured on HotSpot 25.0.3: `JAVA_INT_UNALIGNED`
/// and `JAVA_LONG.withByteAlignment(4)` support exactly `GET` / `SET`.
pub fn segment_vh_form_class_is_aligned(name: &str) -> bool {
    name.ends_with("Aligned")
        || matches!(
            name,
            "java/lang/invoke/VarHandleSegmentAsBytes"
                | "java/lang/invoke/VarHandleSegmentAsBooleans"
        )
}

/// [`segment_vh_form_shape`]'s class-name table.
pub fn segment_vh_shape_of_form_class(name: &str) -> Option<SegShape> {
    let kind = name.strip_prefix("java/lang/invoke/VarHandleSegmentAs")?;
    let kind = kind.strip_suffix("Aligned").unwrap_or(kind);
    Some(match kind {
        "Bytes" => SegShape::Byte,
        "Booleans" => SegShape::Boolean,
        "Shorts" => SegShape::Short,
        "Chars" => SegShape::Char,
        "Ints" => SegShape::Int,
        "Longs" => SegShape::Long,
        "Floats" => SegShape::Float,
        "Doubles" => SegShape::Double,
        _ => return None,
    })
}

/// Does a real `SegmentVarHandle` take the VARIABLE-offset form's extra
/// `long` coordinate (`(MemorySegment, long base, long offset)`)? Read off
/// the form's `GET` method type (`methodType_table[0]`, as
/// `SegmentVarHandle.accessModeTypeUncached` does); `false` when unreadable.
pub fn segment_vh_form_is_variable(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return false;
    };
    let Value::Object(Some(table)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "methodType_table")
    else {
        return false;
    };
    if !ctx.object_is_array(table) || ctx.array_length(table) == 0 {
        return false;
    }
    let Value::Object(Some(get_type)) = ctx.get_array_element(table, 0) else {
        return false;
    };
    match ctx.get_field_by_name(get_type, "ptypes") {
        Value::Object(Some(ptypes)) if ctx.object_is_array(ptypes) => ctx.array_length(ptypes) == 3,
        _ => false,
    }
}

/// JDK 25 `AbstractMemorySegmentImpl.checkEnclosingLayout(base, enclosing, ro)`
/// minus its read-only half (the caller asks that first): the whole
/// `enclosing` layout must fit at `base` (`IndexOutOfBoundsException`), and
/// `base` must meet its alignment in this segment (`IllegalArgumentException`;
/// a heap segment promises at most its element size, `isAlignedForElement`).
/// A synthetic `Unsafe` arena handle is not an address and is not
/// alignment-checked (as in [`segment_vh_misaligned`]).
pub fn segment_vh_check_enclosing(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh: ObjectRef,
    enclosing_size: i64,
    enclosing_align: i64,
    base: i64,
) -> Result<(), MethodCallFailed> {
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    let real = is_real_foreign_segment(ctx, seg);
    let fits = base >= 0 && base <= size.saturating_sub(enclosing_size.max(0));
    if !fits {
        // SBR3 lane ffm: the JDK's words
        // (`AbstractMemorySegmentImpl.outOfBoundException`), naming the
        // segment (j27 wave 4, lane FFMMSG: a carrier too; it said
        // `offset=.., length=.., size=..`).
        let text = segment_refusal_text(ctx, seg);
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(crate::panama::out_of_bounds_message(&text, base, enclosing_size)),
        }
        .into());
    }
    if enclosing_align <= 1 {
        return Ok(());
    }
    if crate::panama::heap_segment_view(ctx, seg).is_none() {
        let addr = crate::panama_libffi::segment_address(ctx, seg).wrapping_add(base);
        // An arena handle is 16-aligned (`reserved_for`), as a `malloc` block
        // is, so a REAL native segment over one is alignment-checked like any
        // address (the JDK's `isAlignedForElement` over `unsafeGetOffset`);
        // a carrier's handle keeps the old exemption.
        if !crate::panama::ffm_native_align_check() || (!real && crate::unsafe_arena_contains(addr))
        {
            return Ok(());
        }
    }
    if crate::panama::pe_segment_max_byte_alignment_at(ctx, seg, base) < enclosing_align {
        // j27 wave 4 (lane FFMMSG): a carrier gets HotSpot's text too.
        return Err(enclosing_misaligned(ctx, seg, Some(vh), base, enclosing_align));
    }
    Ok(())
}

/// Is `seg` an instance of one of the JDK's own `jdk/internal/foreign/*`
/// segment classes (as opposed to this VM's `MemorySegmentImpl` carrier)?
pub fn is_real_foreign_segment(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(seg))
        .is_some_and(|n| n.starts_with("jdk/internal/foreign/"))
}

/// The segment's `toString()` for a refusal message (j27 wave 4, lane
/// FFMMSG): a real JDK segment's own (runs Java), a carrier's from
/// `panama::segment_to_string` (and that as the fallback).
pub fn segment_refusal_text(ctx: &mut dyn NativeContext, seg: ObjectRef) -> String {
    let real = is_real_foreign_segment(ctx, seg)
        .then(|| jdk_object_to_string(ctx, seg))
        .flatten();
    real.unwrap_or_else(|| crate::panama::segment_to_string(ctx, seg))
}

/// The `enclosing` layout of a real `SegmentVarHandle` (`None` for any other
/// handle object, whose slots this must not read positionally).
pub fn vh_enclosing_layout(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<ObjectRef> {
    let is_segment_vh = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(vh))
        .is_some_and(|n| &*n == SEGMENT_VAR_HANDLE_CLASS);
    if !is_segment_vh {
        return None;
    }
    segment_vh_fields(ctx, vh).map(|fields| fields.0)
}

/// `checkEnclosingLayout`'s `Target offset %d is incompatible with alignment
/// constraint %d (of %s) for segment %s` (j27 wave 4, lane FFMMSG; the real
/// segment's text was SBR3's). The layout is read off the handle before any
/// Java runs and pinned across the segment's `toString` (a GC during it
/// moves the layout).
pub fn enclosing_misaligned(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh: Option<ObjectRef>,
    offset: i64,
    align: i64,
) -> MethodCallFailed {
    let layout = vh.and_then(|vh| vh_enclosing_layout(ctx, vh));
    let pinned = layout.map(|layout| (ctx.pin_native_root(layout), layout));
    let segment = segment_refusal_text(ctx, seg);
    let layout = pinned.map(|(pin, layout)| {
        let now = ctx.read_native_pin(pin, layout);
        ctx.unpin_native_roots(pin);
        now
    });
    RuntimeError::IllegalArgumentException {
        message: crate::panama::misaligned_message(ctx, &segment, layout, offset, align),
    }
    .into()
}

/// `obj.toString()` through the JDK's own bytecode, for an error message.
pub fn jdk_object_to_string(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<String> {
    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// A `long` coordinate of a VarHandle call.
pub fn vh_long_arg(args: &[Value], index: usize) -> i64 {
    match args.get(index) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    }
}

/// [`segment_vh_get`] / [`segment_vh_set`] under [`segment_vh_form_enabled`]:
/// `shape` is the form's value type; `args` is `[vh, seg, base, (offset,)?
/// (value)?]`. `enclosing`'s size and alignment are read before anything can
/// run Java.
#[allow(clippy::too_many_arguments)]
pub fn segment_vh_form_access(
    ctx: &mut dyn NativeContext,
    plan: SegVhPlan,
    vh: ObjectRef,
    seg: ObjectRef,
    args: &[Value],
    write: bool,
) -> MethodCallResult {
    let SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        ..
    } = plan;
    // `[vh, seg, base]` + the variable form's `offset` + a write's value.
    let variable = args.len() == 4 + usize::from(write);
    let base = vh_long_arg(args, 2);
    let offset = if variable {
        vh_long_arg(args, 3)
    } else {
        vh_offset
    };
    let width = seg_shape_width(shape);
    if write {
        let read_only = crate::phases_late::foreign_ffm::p67_segment_is_read_only(
            ctx,
            &[Value::Object(Some(seg))],
        )?;
        if matches!(read_only, Some(Value::Int(n)) if n != 0) {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Attempt to write a read-only segment".into(),
            }
            .into());
        }
    }
    segment_vh_check_enclosing(ctx, seg, vh, enclosing_size, enclosing_align, base)?;
    let raw_in = if write {
        let value = args
            .get(3 + usize::from(variable))
            .copied()
            .unwrap_or(Value::Int(0));
        let raw = seg_encode_value(&value, width);
        Some(if be { seg_swap_bytes(raw, width) } else { raw })
    } else {
        None
    };
    // An access window from before the session check to after the last
    // touch of the block (see `ffm_fast::AccessWindow`).
    let window = crate::ffm_fast::AccessWindow::open();
    let mut seg = seg;
    segment_vh_check_scope(ctx, &mut seg)?;
    // Re-derived after the session check, which can run Java and move a heap
    // segment's array. `enclosing` covers `[base, base + size)`, so this
    // bounds check only guards a malformed handle.
    let (heap_base, addr) = segment_raw_access(ctx, seg, offset, base, width)?;
    let heap_bounds = || {
        MethodCallFailed::from(RuntimeError::IllegalStateException {
            message: "Out of bound access on heap MemorySegment".into(),
        })
    };
    match raw_in {
        Some(raw) => {
            match heap_base {
                None => segment_native_set(addr, width, raw)?,
                Some(array) => {
                    if !segment_heap_set(ctx, array, addr, width, raw) {
                        return Err(heap_bounds());
                    }
                }
            }
            drop(window);
            Ok(None)
        }
        None => {
            let raw = match heap_base {
                None => segment_native_get(addr, width)?,
                Some(array) => segment_heap_get(ctx, array, addr, width).ok_or_else(heap_bounds)?,
            };
            drop(window);
            let raw = if be { seg_swap_bytes(raw, width) } else { raw };
            // CANONICAL, as the enclosing-derived reading below: the variant
            // and the descriptor switch on the same `SegShape`.
            Ok(Some(box_value_canonical(
                ctx,
                seg_decode_value(shape, raw),
                seg_shape_desc(shape),
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// SBR3 lane ffmperf: the lean access of a REAL native segment
// ---------------------------------------------------------------------------
//
// Once the JDK's own `Arena` / `NativeMemorySegmentImpl` run, `seg.get(JAVA_INT,
// off)` is `accessHandle().get(this, off)` on a real `SegmentVarHandle`, and the
// carriers' one-native fast accessor (80 ns per get+set) became the generic
// `VarHandle` native road (3.9 us: the access-mode prologue, three field-index
// resolutions by NAME per access, class-name `Arc`s, the arena store's lock taken
// twice). This is the same access `segment_vh_form_access` performs, decided
// from the handle's memoised [`SegVhPlan`], the segment class's memoised field
// slots, the session class's memoised `state`/`owner` slots and ONE arena-store
// call -- and it only ever ANSWERS: any condition it cannot prove (a heap
// segment, a carrier, a variable-offset form, a read-only or out-of-bounds or
// misaligned access, a closed or foreign-owned session, a freed handle, a value
// of another type) declines to `None` and the existing road runs and raises
// exactly what it always did. `CRATONVM_FFM_SEGMENT_VH_LEAN=0` turns it off.

#[derive(Clone, Copy)]
pub struct SegLeanClass {
    pub scope: usize,
    pub length: usize,
    pub read_only: usize,
    pub min: usize,
}

#[derive(Clone, Copy)]
pub struct SegLeanSession {
    pub state: usize,
    pub owner: usize,
}

pub const SEG_LEAN_MEMO_WAYS: usize = 8;

thread_local! {
    /// `(vm, class, slots)` of the segment classes the lean path serves
    /// (`None`: another class, which declines).
    pub static SEG_LEAN_CLASS_MEMO: std::cell::RefCell<Vec<(usize, ClassId, Option<SegLeanClass>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `(vm, class, slots)` of the real session classes.
    pub static SEG_LEAN_SESSION_MEMO: std::cell::RefCell<Vec<(usize, ClassId, Option<SegLeanSession>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub fn seg_lean_memo<T: Copy>(
    memo: &'static std::thread::LocalKey<std::cell::RefCell<Vec<(usize, ClassId, Option<T>)>>>,
    vm: usize,
    class_id: ClassId,
    compute: impl FnOnce() -> Option<T>,
) -> Option<T> {
    let hit = memo.with(|m| {
        m.borrow()
            .iter()
            .find(|e| e.0 == vm && e.1 == class_id)
            .map(|e| e.2)
    });
    if let Some(found) = hit {
        return found;
    }
    let value = compute();
    memo.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= SEG_LEAN_MEMO_WAYS {
            m.remove(0);
        }
        m.push((vm, class_id, value));
    });
    value
}

pub fn seg_lean_class(ctx: &dyn NativeContext, class_id: ClassId) -> Option<SegLeanClass> {
    seg_lean_memo(&SEG_LEAN_CLASS_MEMO, ctx.vm_identity(), class_id, || {
        let name = ctx.class_name_arc_of_id(class_id)?;
        if !matches!(
            &*name,
            "jdk/internal/foreign/NativeMemorySegmentImpl"
                | "jdk/internal/foreign/MappedMemorySegmentImpl"
        ) {
            return None;
        }
        Some(SegLeanClass {
            scope: ctx.resolve_field_index_by_class_id(class_id, "scope")?,
            length: ctx.resolve_field_index_by_class_id(class_id, "length")?,
            read_only: ctx.resolve_field_index_by_class_id(class_id, "readOnly")?,
            min: ctx.resolve_field_index_by_class_id(class_id, "min")?,
        })
    })
}

pub fn seg_lean_session(ctx: &dyn NativeContext, class_id: ClassId) -> Option<SegLeanSession> {
    seg_lean_memo(&SEG_LEAN_SESSION_MEMO, ctx.vm_identity(), class_id, || {
        // A real session (the VM's own `MemorySessionImpl` shape is not one,
        // see `p67_session_is_real`).
        if ctx.class_name_arc_of_id(class_id).as_deref()
            == Some("jdk/internal/foreign/MemorySessionImpl")
        {
            return None;
        }
        Some(SegLeanSession {
            state: ctx.resolve_field_index_by_class_id(class_id, "state")?,
            owner: ctx.resolve_field_index_by_class_id(class_id, "owner")?,
        })
    })
}

/// The final fields of one real native segment (`length`, `readOnly`, `scope`,
/// `min` never change after construction) and its session's field slots,
/// valid for one address in one [`crate::ffm_fast::epoch`] -- the epoch moves
/// on every collection (the only thing that moves a segment or its session)
/// and on every free of native memory, so an address stands for its object in
/// between, the argument `ffm_fast`'s carrier verdicts rest on. What is NOT
/// cached is anything a close can change: the session's `state` and `owner`
/// are read on every access.
#[derive(Clone, Copy)]
pub struct SegLeanFacts {
    pub vm: usize,
    pub addr: usize,
    pub epoch: u64,
    pub size: i64,
    pub read_only: bool,
    pub min: i64,
    pub scope: ObjectRef,
    pub session: SegLeanSession,
}

pub const SEG_LEAN_FACT_WAYS: usize = 4;

thread_local! {
    pub static SEG_LEAN_FACTS: std::cell::Cell<[Option<SegLeanFacts>; SEG_LEAN_FACT_WAYS]> =
        const { std::cell::Cell::new([None; SEG_LEAN_FACT_WAYS]) };
    pub static SEG_LEAN_FACTS_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn seg_lean_facts(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    class_hint: Option<ClassId>,
) -> Option<SegLeanFacts> {
    let vm = ctx.vm_identity();
    let addr = seg.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    let ways = SEG_LEAN_FACTS.with(std::cell::Cell::get);
    for way in ways.iter().flatten() {
        if way.addr == addr && way.epoch == epoch && way.vm == vm {
            return Some(*way);
        }
    }
    let class_id = match class_hint {
        Some(id) => id,
        None => ctx.class_id_of_object(seg),
    };
    let class = seg_lean_class(&*ctx, class_id)?;
    let mut fields = [Value::Int(0); 4];
    ctx.get_fields_typed(
        seg,
        &[
            (class.length, b'J'),
            (class.read_only, b'Z'),
            (class.scope, b'L'),
            (class.min, b'J'),
        ],
        &mut fields,
    );
    let (Value::Long(size), Value::Int(read_only), Value::Object(Some(scope)), Value::Long(min)) =
        (fields[0], fields[1], fields[2], fields[3])
    else {
        return None;
    };
    let session = seg_lean_session(&*ctx, ctx.class_id_of_object(scope))?;
    let facts = SegLeanFacts {
        vm,
        addr,
        epoch,
        size,
        read_only: read_only != 0,
        min,
        scope,
        session,
    };
    let slot = SEG_LEAN_FACTS_NEXT.with(|c| {
        let i = c.get();
        c.set((i + 1) % SEG_LEAN_FACT_WAYS);
        i
    });
    let mut updated = ways;
    updated[slot] = Some(facts);
    SEG_LEAN_FACTS.with(|c| c.set(updated));
    Some(facts)
}

/// `CRATONVM_FFM_SEGMENT_VH_LEAN` (default on): see the block comment above.
pub fn segment_vh_lean_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The JVM descriptor character a call site declares for `shape`'s carrier.
pub fn seg_shape_site_kind(shape: SegShape) -> u8 {
    match shape {
        SegShape::Byte => b'B',
        SegShape::Boolean => b'Z',
        SegShape::Short => b'S',
        SegShape::Char => b'C',
        SegShape::Int => b'I',
        SegShape::Long | SegShape::Address => b'J',
        SegShape::Float => b'F',
        SegShape::Double => b'D',
    }
}

/// One planned `get` (`write == None`: the decoded value comes back) or `set`
/// (`Some(value)`: `Some(None)` comes back) of a real native segment, or `None`
/// to decline. `site_kind` is the call site's declared carrier (`0`: not
/// known, the caller has judged the value itself).
pub fn segment_vh_lean(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    seg: ObjectRef,
    base: i64,
    write: Option<Value>,
    site_kind: u8,
    keys: Option<(i32, ClassId)>,
) -> Option<(SegShape, Option<Value>)> {
    if !segment_vh_lean_enabled() || !segment_vh_checks() || !segment_vh_form_enabled() {
        return None;
    }
    // The funnel already knows the handle's identity hash and the segment's
    // class (its per-thread receiver memo); a native road asks the context.
    let plan = match keys {
        Some((hash, _)) => seg_vh_plan_hit(ctx.vm_identity(), hash, vh.as_ptr() as usize)?,
        None => vh_memo_lookup(&SEG_VH_PLAN_MEMO, &*ctx, vh, |_| None)?,
    };
    segment_vh_lean_planned(ctx, plan, seg, base, write, site_kind, keys.map(|k| k.1))
}

/// [`segment_vh_lean`] once the handle's plan is in hand (SBR4 lane segacc: the
/// compiled `MemorySegment.get/set` door has the LAYOUT's plan, not a handle).
pub fn segment_vh_lean_planned(
    ctx: &mut dyn NativeContext,
    plan: SegVhPlan,
    seg: ObjectRef,
    base: i64,
    write: Option<Value>,
    site_kind: u8,
    class_hint: Option<ClassId>,
) -> Option<(SegShape, Option<Value>)> {
    if plan.variable {
        return None;
    }
    let SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        ..
    } = plan;
    if site_kind != 0 && seg_shape_site_kind(shape) != site_kind {
        return None;
    }
    let width = seg_shape_width(shape);
    let raw_in = match write {
        Some(value) => {
            let fits = matches!(
                (shape, &value),
                (
                    SegShape::Byte
                        | SegShape::Boolean
                        | SegShape::Short
                        | SegShape::Char
                        | SegShape::Int,
                    Value::Int(_)
                ) | (SegShape::Long | SegShape::Address, Value::Long(_))
                    | (SegShape::Float, Value::Float(_))
                    | (SegShape::Double, Value::Double(_))
            );
            if !fits {
                return None;
            }
            let raw = seg_encode_value(&value, width);
            Some(if be { seg_swap_bytes(raw, width) } else { raw })
        }
        None => None,
    };
    // r23 wave 3, lane L8: a heap segment this thread already filed is
    // served before the native facts are asked for (they miss on every heap
    // access: no `min`, and a miss files nothing).
    if crate::nbflags().ffm_segment_vh_lean_heap && seg_heap_facts_filed(ctx, seg) {
        let out = segment_vh_lean_heap(ctx, &plan, seg, base, raw_in)?;
        return Some((shape, out));
    }
    let Some(SegLeanFacts {
        size,
        read_only,
        min,
        scope,
        session,
        ..
    }) = seg_lean_facts(ctx, seg, class_hint)
    else {
        // r23 wave 2c, lane L8: not a native segment -- a real heap one
        // (`MemorySegment.ofArray`) is served through its backing array.
        if !crate::nbflags().ffm_segment_vh_lean_heap {
            return None;
        }
        let out = segment_vh_lean_heap(ctx, &plan, seg, base, raw_in)?;
        return Some((shape, out));
    };
    if raw_in.is_some() && read_only {
        return None;
    }
    // The JDK's checks in the JDK's order, all of them passing or the
    // existing road reports the failure.
    if base < 0 || base > size.saturating_sub(enclosing_size.max(0)) {
        return None;
    }
    let total = vh_offset.checked_add(base)?;
    if total < 0 || total.checked_add(width)? > size {
        return None;
    }
    if enclosing_align > 1 && min.wrapping_add(base) & (enclosing_align - 1) != 0 {
        return None;
    }
    let addr = min.checked_add(total)?;
    // An arena handle (what the JDK's `allocateMemory` answers on this VM) goes
    // through the arena store; a plain non-null pointer (a mapped file, an
    // `ofAddress` window) is dereferenced as `segment_native_get` does. A freed
    // or foreign tagged value declines.
    let handle = crate::panama_libffi::is_arena_handle(addr);
    if !handle && (addr <= 0 || crate::unsafe_arena_addr_is_tagged(addr)) {
        return None;
    }
    // An access window from before the session check to after the last touch
    // of the block (see `ffm_fast::AccessWindow`).
    let window = crate::ffm_fast::AccessWindow::open();
    let mut state_owner = [Value::Int(0); 2];
    ctx.get_fields_typed(
        scope,
        &[(session.state, b'I'), (session.owner, b'L')],
        &mut state_owner,
    );
    match state_owner[0] {
        Value::Int(state) if state >= 0 => {}
        _ => return None,
    }
    match state_owner[1] {
        Value::Object(None) => {}
        Value::Object(Some(owner)) => {
            // zs s1/nd: the EXISTING mirror only (`current_thread_object_if_any`,
            // as `foreign_ffm.rs`'s owner check): a thread with no mirror yet
            // cannot own this session, and minting one here is an allocation
            // from a JIT helper (`try_varhandle_segment_access`), where the
            // one-attempt door aborts on a full ZGC heap.
            if ctx.current_thread_object_if_any() != Some(owner) {
                return None;
            }
        }
        _ => return None,
    }
    let out = match raw_in {
        Some(raw) => {
            if handle {
                let bytes = raw.to_le_bytes();
                if !crate::unsafe_arena_copy_in(addr, &bytes[..width as usize]) {
                    return None;
                }
            } else {
                // SAFETY: the address is the segment's own `min + offset`, inside
                // the segment's bounds (checked above), as in `segment_native_set`.
                unsafe {
                    let ptr = addr as usize as *mut u8;
                    match width {
                        1 => *ptr = raw as u8,
                        2 => (ptr as *mut u16).write_unaligned(raw as u16),
                        4 => (ptr as *mut u32).write_unaligned(raw as u32),
                        _ => (ptr as *mut u64).write_unaligned(raw),
                    }
                }
            }
            None
        }
        None => {
            let raw = if handle {
                let mut buf = [0u8; 8];
                if !crate::unsafe_arena_copy_out(addr, &mut buf[..width as usize]) {
                    return None;
                }
                u64::from_le_bytes(buf)
            } else {
                // SAFETY: see the store above; `segment_native_get`'s own read.
                unsafe {
                    let ptr = addr as usize as *const u8;
                    match width {
                        1 => *ptr as u64,
                        2 => (ptr as *const u16).read_unaligned() as u64,
                        4 => (ptr as *const u32).read_unaligned() as u64,
                        _ => (ptr as *const u64).read_unaligned(),
                    }
                }
            };
            let raw = if be { seg_swap_bytes(raw, width) } else { raw };
            Some(seg_decode_value(shape, raw))
        }
    };
    drop(window);
    Some((shape, out))
}

/// JIT round 23 wave 2c, lane L8
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// the final facts of one real HEAP segment (`HeapMemorySegmentImpl$Of*`: its
/// backing array, data start, size and read-only flag never change) and its
/// session's field slots, filed per thread for one segment address in one
/// [`crate::ffm_fast::epoch`] -- the argument [`SegLeanFacts`] rests on (the
/// epoch moves on every collection, the only thing that moves the segment or
/// its array). The session's `state` and `owner` are read on every access.
#[derive(Clone, Copy)]
pub struct SegHeapFacts {
    pub vm: usize,
    pub addr: usize,
    pub epoch: u64,
    pub(crate) view: crate::panama::HeapSegmentView,
    pub scope: ObjectRef,
    pub session: SegLeanSession,
}

pub const SEG_HEAP_FACT_WAYS: usize = 4;

thread_local! {
    pub static SEG_HEAP_FACTS: std::cell::Cell<[Option<SegHeapFacts>; SEG_HEAP_FACT_WAYS]> =
        const { std::cell::Cell::new([None; SEG_HEAP_FACT_WAYS]) };
    pub static SEG_HEAP_FACTS_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Is `seg` in this thread's [`SegHeapFacts`] lines for the current epoch?
/// Reads only (r23 wave 3, lane L8).
pub fn seg_heap_facts_filed(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    let vm = ctx.vm_identity();
    let addr = seg.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    SEG_HEAP_FACTS.with(std::cell::Cell::get).iter().flatten().any(|way| {
        way.addr == addr && way.epoch == epoch && way.vm == vm
    })
}

pub fn seg_heap_facts(ctx: &mut dyn NativeContext, seg: ObjectRef) -> Option<SegHeapFacts> {
    let vm = ctx.vm_identity();
    let addr = seg.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    let ways = SEG_HEAP_FACTS.with(std::cell::Cell::get);
    for way in ways.iter().flatten() {
        if way.addr == addr && way.epoch == epoch && way.vm == vm {
            return Some(*way);
        }
    }
    // A REAL heap segment only: this VM's own `ofArray` carriers keep their
    // older road.
    if !crate::panama_libffi::is_real_heap_segment(&*ctx, seg) {
        return None;
    }
    let view = crate::panama::heap_segment_view(&*ctx, seg)?;
    let Value::Object(Some(scope)) = real_object_field(
        ctx,
        seg,
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "scope",
    ) else {
        return None;
    };
    let session = seg_lean_session(&*ctx, ctx.class_id_of_object(scope))?;
    let facts = SegHeapFacts {
        vm,
        addr,
        epoch,
        view,
        scope,
        session,
    };
    let slot = SEG_HEAP_FACTS_NEXT.with(|c| {
        let i = c.get();
        c.set((i + 1) % SEG_HEAP_FACT_WAYS);
        i
    });
    let mut updated = ways;
    updated[slot] = Some(facts);
    SEG_HEAP_FACTS.with(|c| c.set(updated));
    Some(facts)
}

/// [`segment_vh_lean_planned`]'s heap arm: one planned `get` (`raw_in ==
/// None`: `Some(Some(value))`) or `set` (`raw_in`: the value's memory image,
/// already byte-swapped for a big-endian handle; `Some(None)`) of a real heap
/// segment, or `None` to decline -- any check that does not pass declines, and
/// the full road ([`segment_vh_get`] / its `set` twin) then raises exactly
/// what it always raised. The JDK's checks, in its order: read-only, bounds,
/// alignment (a layout aligned wider than the backing element is the heap
/// segment's `maxAlignMask` refusal, so it declines too), then the session.
///
/// The bytes are the backing array's, little-endian through the element and
/// through the array (`panama::heap_bytes_read`'s measured rule); an access as
/// wide as the element and aligned to it is one element read or write.
pub fn segment_vh_lean_heap(
    ctx: &mut dyn NativeContext,
    plan: &SegVhPlan,
    seg: ObjectRef,
    base: i64,
    raw_in: Option<u64>,
) -> Option<Option<Value>> {
    let SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        ..
    } = *plan;
    let width = seg_shape_width(shape);
    let SegHeapFacts {
        view,
        scope,
        session,
        ..
    } = seg_heap_facts(ctx, seg)?;
    let size = view.size();
    if raw_in.is_some() && view.read_only() {
        return None;
    }
    if base < 0 || base > size.saturating_sub(enclosing_size.max(0)) {
        return None;
    }
    let total = vh_offset.checked_add(base)?;
    if total < 0 || total.checked_add(width)? > size {
        return None;
    }
    // Cast: an element is 1..=8 bytes.
    let elem_width = view.elem_width as i64;
    let byte_start = view.start.checked_add(total)?;
    if enclosing_align > elem_width
        || (enclosing_align > 1 && byte_start & (enclosing_align - 1) != 0)
    {
        return None;
    }
    let mut state_owner = [Value::Int(0); 2];
    ctx.get_fields_typed(
        scope,
        &[(session.state, b'I'), (session.owner, b'L')],
        &mut state_owner,
    );
    match state_owner[0] {
        Value::Int(state) if state >= 0 => {}
        _ => return None,
    }
    match state_owner[1] {
        Value::Object(None) => {}
        Value::Object(Some(owner)) => {
            if ctx.current_thread_object_if_any() != Some(owner) {
                return None;
            }
        }
        _ => return None,
    }
    // Cast: `byte_start >= 0` (the view's start and `total` are), and the
    // view proved the whole segment lies inside the array.
    let whole_element = width == elem_width && byte_start % elem_width == 0;
    let index = (byte_start / elem_width) as usize;
    let mask = if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * width)) - 1
    };
    match raw_in {
        Some(raw) => {
            if whole_element {
                crate::panama::heap_store_element(&*ctx, view.base, view.elem_type, index, raw);
            } else {
                crate::panama::heap_bytes_write(
                    &*ctx,
                    view.base,
                    view.elem_type,
                    view.elem_width,
                    byte_start,
                    width as usize,
                    raw,
                );
            }
            Some(None)
        }
        None => {
            let raw = if whole_element {
                crate::panama::heap_element_bits(&*ctx, view.base, index) & mask
            } else {
                crate::panama::heap_bytes_read(
                    &*ctx,
                    view.base,
                    view.elem_width,
                    byte_start,
                    width as usize,
                )
            };
            let raw = if be { seg_swap_bytes(raw, width) } else { raw };
            Some(Some(seg_decode_value(shape, raw)))
        }
    }
}

/// r23 wave 2g, lane L8
/// (`docs/feature-designs/native-dispatch-architecture.md`;
/// correctness, `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE`, default ON): a real
/// `SegmentVarHandle` `get` / `set` whose native road REFUSED (alignment,
/// bounds, read-only, a closed session, a null segment) is run again through
/// the JDK's own static form body -- `VarHandleSegmentAs<X>.get(VarHandle,
/// Object, long)` / `set(VarHandle, Object, long, x)`, what HotSpot's
/// `VarHandleGuards` links to -- so the JDK raises the exception itself with
/// its own frames (`checkSegment`, `checkEnclosingLayout`, `checkAccess`,
/// `checkBounds`, `Preconditions.*`, `ScopedMemoryAccess.*`). The refusal's
/// own exception is dropped. Only the throwing path pays; `None` (keep the
/// native's answer) for any other mode, a variable-offset form, an
/// unreadable form or an argument shape the body does not take. A body that
/// does NOT throw answers what it computed (boxed for a `get`, as the native
/// road answers).
pub fn segment_vh_raise_through_jdk(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    mode: &str,
) -> Option<MethodCallResult> {
    if !crate::nbflags().ffm_segment_vh_jdk_raise {
        return None;
    }
    // j27 wave 1 (lane FFMVH): never raise through the JDK from inside a
    // raise. A body that resolves back to a refusing segment access (a mode
    // the form class does not declare, see `segment_vh_mode_descriptor`)
    // keeps the native's own exception instead of recursing to a stack
    // overflow.
    if SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()) {
        return None;
    }
    let _in_flight = SegVhRaiseInFlight::enter();
    // JIT r23 wave 6 (lane L8, `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE_ALL_MODES`):
    // the ordered and read-modify-write modes too, through the same static
    // form bodies (declared on `VarHandleSegmentAs<X>Aligned`).
    let kind = if crate::nbflags().ffm_segment_vh_jdk_raise_all_modes {
        segment_vh_mode_kind(mode)?
    } else {
        match mode {
            "get" => SegModeKind::Get,
            "set" => SegModeKind::Set,
            _ => return None,
        }
    };
    if !matches!(mode, "get" | "set") {
        return segment_vh_raise_mode_through_jdk(ctx, vh, args, mode, kind);
    }
    let is_set = kind == SegModeKind::Set;
    if args.len() != 3 + usize::from(is_set) || !matches!(args.get(2), Some(Value::Long(_))) {
        return None;
    }
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return None;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "implClass")
    else {
        return None;
    };
    let impl_class = ctx.class_id_from_mirror(impl_mirror)?;
    let name = ctx.class_name_of_id(impl_class)?;
    let shape = segment_vh_shape_of_form_class(&name)?;
    if segment_vh_form_is_variable(ctx, vh) {
        return None;
    }
    // The static `get` / `set` are declared on the base form class; the
    // `...Aligned` subclass adds the ordered modes only.
    let owner = name.strip_suffix("Aligned").unwrap_or(&name).to_string();
    let carrier = seg_shape_desc(shape);
    let value_fits = !is_set
        || matches!(
            (carrier, args.get(3)),
            ("B" | "Z" | "S" | "C" | "I", Some(Value::Int(_)))
                | ("J", Some(Value::Long(_)))
                | ("F", Some(Value::Float(_)))
                | ("D", Some(Value::Double(_)))
        );
    if !value_fits {
        return None;
    }
    let desc = if is_set {
        format!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J{carrier})V")
    } else {
        format!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J){carrier}")
    };
    let out = ctx.invoke_static(&owner, mode, &desc, args);
    Some(match out {
        Ok(Some(v)) if !is_set => Ok(Some(box_value_canonical(ctx, v, carrier))),
        Ok(_) if is_set => Ok(None),
        other => other,
    })
}

thread_local! {
    /// j27 wave 1 (lane FFMVH): a [`segment_vh_raise_through_jdk`] is running
    /// on this thread.
    pub static SEG_VH_RAISE_IN_FLIGHT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Holds [`SEG_VH_RAISE_IN_FLIGHT`] for one raise; clears it on drop (also
/// on an unwind).
pub struct SegVhRaiseInFlight;

impl SegVhRaiseInFlight {
    pub fn enter() -> Self {
        SEG_VH_RAISE_IN_FLIGHT.with(|c| c.set(true));
        SegVhRaiseInFlight
    }
}

impl Drop for SegVhRaiseInFlight {
    fn drop(&mut self) {
        SEG_VH_RAISE_IN_FLIGHT.with(|c| c.set(false));
    }
}

/// The shape of a `VarHandle` access mode's static form body: what it reads
/// and returns (JDK 25 `VarHandleSegmentAs<X>[Aligned]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegModeKind {
    /// `get`, `getVolatile`, `getAcquire`, `getOpaque`: no value, returns `x`.
    Get,
    /// `set`, `setVolatile`, `setRelease`, `setOpaque`: one value, `void`.
    Set,
    /// `compareAndSet`, `weakCompareAndSet[Plain|Acquire|Release]`: expected
    /// and new value, returns `boolean`.
    Cas,
    /// `compareAndExchange[Acquire|Release]`: expected and new, returns `x`.
    Cae,
    /// `getAndSet*`: one value, returns `x`; any carrier the form serves.
    GetAndSet,
    /// `getAndAdd*`, `getAndBitwise{Or,And,Xor}*`: one value, returns `x`;
    /// integral carriers only (the float forms have no such body).
    GetAndArith,
}

/// [`SegModeKind`] of an access-mode name, or `None` for any other name.
pub fn segment_vh_mode_kind(mode: &str) -> Option<SegModeKind> {
    Some(match mode {
        "get" | "getVolatile" | "getAcquire" | "getOpaque" => SegModeKind::Get,
        "set" | "setVolatile" | "setRelease" | "setOpaque" => SegModeKind::Set,
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => SegModeKind::Cas,
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => {
            SegModeKind::Cae
        }
        "getAndSet" | "getAndSetAcquire" | "getAndSetRelease" => SegModeKind::GetAndSet,
        "getAndAdd" | "getAndAddAcquire" | "getAndAddRelease" | "getAndBitwiseOr"
        | "getAndBitwiseOrAcquire" | "getAndBitwiseOrRelease" | "getAndBitwiseAnd"
        | "getAndBitwiseAndAcquire" | "getAndBitwiseAndRelease" | "getAndBitwiseXor"
        | "getAndBitwiseXorAcquire" | "getAndBitwiseXorRelease" => SegModeKind::GetAndArith,
        _ => return None,
    })
}

/// The static form body's descriptor for `kind` over `carrier` (constant
/// offset form: `(VarHandle, Object, long, values...)`), or `None` when JDK 25
/// declares no such body (`getAndAdd` / `getAndBitwise*` of `F` / `D`).
pub fn segment_vh_mode_descriptor(kind: SegModeKind, carrier: &str) -> Option<String> {
    const HEAD: &str = "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J";
    // j27 wave 1 (lane FFMVH; page
    // `ffm-segment-varhandle-short-compare-and-set-overflows-the-stack-20261027`):
    // JDK 25 generates the CAS family (`compareAndSet`, `compareAndExchange*`,
    // `weakCompareAndSet*`, `getAndSet*`) only for the `int` / `long` /
    // `float` / `double` segment views (`GensrcVarHandles.gmk`: the byte,
    // short and char views have no `CAS` key). `VarHandleSegmentAsShortsAligned`
    // and `...CharsAligned` exist -- they add the ORDERED modes -- so a
    // descriptor minted for them named a body that is not there, and
    // `invoke_static` resolved the name to the signature-polymorphic
    // `VarHandle.compareAndSet`, whose native refused and raised through here
    // again until the stack overflowed (SIGSEGV where HotSpot throws
    // `UnsupportedOperationException: compareAndSet`).
    let cas_family = matches!(carrier, "I" | "J" | "F" | "D");
    Some(match kind {
        SegModeKind::Get => format!("{HEAD}){carrier}"),
        SegModeKind::Set => format!("{HEAD}{carrier})V"),
        SegModeKind::Cas | SegModeKind::Cae | SegModeKind::GetAndSet if !cas_family => {
            return None;
        }
        SegModeKind::Cas => format!("{HEAD}{carrier}{carrier})Z"),
        SegModeKind::Cae => format!("{HEAD}{carrier}{carrier}){carrier}"),
        SegModeKind::GetAndSet => format!("{HEAD}{carrier}){carrier}"),
        SegModeKind::GetAndArith => {
            if !matches!(carrier, "I" | "J") {
                return None;
            }
            format!("{HEAD}{carrier}){carrier}")
        }
    })
}

/// How many values a [`SegModeKind`] body takes after the offset.
pub fn segment_vh_mode_values(kind: SegModeKind) -> usize {
    match kind {
        SegModeKind::Get => 0,
        SegModeKind::Set | SegModeKind::GetAndSet | SegModeKind::GetAndArith => 1,
        SegModeKind::Cas | SegModeKind::Cae => 2,
    }
}

/// JIT r23 wave 6 (lane L8; `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE_ALL_MODES`,
/// correctness, default ON): [`segment_vh_raise_through_jdk`] for an ORDERED
/// or read-modify-write mode of a constant-offset `SegmentVarHandle`. Those
/// bodies exist only on the aligned form class
/// (`VarHandleSegmentAs<X>Aligned`, JDK 25 `javap`), so a handle whose form
/// is not aligned answers `None` and keeps the native's
/// `UnsupportedOperationException`, as does a mode the carrier has no body
/// for. Measured on n23-w5 (`C:\craton\n23p\w6L8\R23W6SegModes.java`): every
/// such failure carried no JDK frame at all, where HotSpot shows
/// `checkEnclosingLayout < SegmentVarHandle.checkSegment <
/// VarHandleSegmentAsIntsAligned.<mode>` x2 (plus the bounds chain).
pub fn segment_vh_raise_mode_through_jdk(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    mode: &str,
    kind: SegModeKind,
) -> Option<MethodCallResult> {
    let values = segment_vh_mode_values(kind);
    if args.len() != 3 + values || !matches!(args.get(2), Some(Value::Long(_))) {
        return None;
    }
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return None;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "implClass")
    else {
        return None;
    };
    let impl_class = ctx.class_id_from_mirror(impl_mirror)?;
    let owner = ctx.class_name_of_id(impl_class)?;
    // j27 wave 1 (lane FFMVH): the bytes / booleans forms have no `Aligned`
    // twin and declare their ordered modes themselves
    // (`segment_vh_form_class_is_aligned`), so a refused `getVolatile` on a
    // byte handle raises through its body too; their CAS family has no body
    // (`segment_vh_mode_descriptor` answers `None`).
    if !segment_vh_form_class_is_aligned(&owner) {
        return None;
    }
    let shape = segment_vh_shape_of_form_class(&owner)?;
    if segment_vh_form_is_variable(ctx, vh) {
        return None;
    }
    let carrier = seg_shape_desc(shape);
    let desc = segment_vh_mode_descriptor(kind, carrier)?;
    let value_fits = args.get(3..).unwrap_or(&[]).iter().all(|v| {
        matches!(
            (carrier, v),
            ("B" | "Z" | "S" | "C" | "I", Value::Int(_))
                | ("J", Value::Long(_))
                | ("F", Value::Float(_))
                | ("D", Value::Double(_))
        )
    });
    if !value_fits {
        return None;
    }
    let out = ctx.invoke_static(&owner, mode, &desc, args);
    Some(match (out, kind) {
        (Ok(_), SegModeKind::Set) => Ok(None),
        (Ok(Some(v)), SegModeKind::Cas) => Ok(Some(box_value_canonical(ctx, v, "Z"))),
        (Ok(Some(v)), _) => Ok(Some(box_value_canonical(ctx, v, carrier))),
        (other, _) => other,
    })
}


/// r23 wave 2d, lane L8: the HEAP twin of the compiled FFM element door
/// ([`segment_layout_facts`] serves only a native segment): one
/// `MemorySegment.getAtIndex/setAtIndex` (or the offset `get/set`, which the
/// emitter has turned into an index) of a real heap segment through `layout`,
/// at a site whose carrier is `site_kind`. `write`: the value's bits (the
/// carrier's low bits). Answers the read's bits (sign- / zero-extended as the
/// native door's `real_seg_serve` answers them; a float's or double's raw
/// bits), `Some(0)` for a served store, or `None` to decline -- the site then
/// runs its unchanged dispatch, which raises whatever it always raised. The
/// admission is [`segment_layout_facts`]'s (a plain access through a built
/// layout handle); the access is [`segment_vh_lean_heap`]'s. Allocates nothing.
pub fn segment_layout_heap_access(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    site_kind: u8,
    index: i64,
    write: Option<i64>,
) -> Option<i64> {
    segment_layout_heap_access_with(ctx, seg, layout, site_kind, write, |width| {
        index.checked_mul(width)
    })
}

/// JIT r23 wave 4 (lane L8, `CRATONVM_JIT_FFM_UNALIGNED_OFFSET_ACCESS`): the
/// byte-offset form of [`segment_layout_heap_access`], for a `get/set(layout,
/// long offset)` site whose offset is not a multiple of the width (an
/// unaligned layout, `withByteAlignment(1)` / `JAVA_INT_UNALIGNED`). The
/// checks are the same ones in the same order -- [`segment_vh_lean_heap`]
/// tests bounds, read-only and the layout's alignment against the byte
/// offset -- so a misaligned access through an ALIGNED layout still declines
/// to the JDK body and its `IllegalArgumentException`.
pub fn segment_layout_heap_access_bytes(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    site_kind: u8,
    byte_offset: i64,
    write: Option<i64>,
) -> Option<i64> {
    segment_layout_heap_access_with(ctx, seg, layout, site_kind, write, |_| Some(byte_offset))
}

pub fn segment_layout_heap_access_with(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    site_kind: u8,
    write: Option<i64>,
    to_base: impl FnOnce(i64) -> Option<i64>,
) -> Option<i64> {
    if !crate::nbflags().ffm_segment_vh_lean_heap
        || !segment_vh_lean_enabled()
        || !segment_vh_checks()
        || !segment_vh_form_enabled()
    {
        return None;
    }
    let plan = seg_layout_plan(ctx, layout)?;
    let width = seg_shape_width(plan.shape);
    if plan.variable
        || plan.vh_offset != 0
        || plan.enclosing_align > width
        || plan.enclosing_size != width
        || seg_shape_site_kind(plan.shape) != site_kind
    {
        return None;
    }
    let base = to_base(width)?;
    let raw_in = write.map(|bits| {
        let mask = if width >= 8 {
            u64::MAX
        } else {
            (1u64 << (8 * width)) - 1
        };
        // Cast: the carrier's bits, reinterpreted.
        let raw = (bits as u64) & mask;
        if plan.be {
            seg_swap_bytes(raw, width)
        } else {
            raw
        }
    });
    match segment_vh_lean_heap(ctx, &plan, seg, base, raw_in)? {
        None => Some(0),
        Some(Value::Int(v)) => Some(i64::from(v)),
        Some(Value::Long(v)) => Some(v),
        Some(Value::Float(f)) => Some(i64::from(f.to_bits())),
        // Cast: the raw bits.
        Some(Value::Double(d)) => Some(d.to_bits() as i64),
        Some(_) => None,
    }
}

/// The native roads' face of [`segment_vh_lean`]: the boxed answer of a `get`
/// (`Some(Some(value))`), a served `set` (`Some(None)`), or a decline (`None`).
/// `args` is `[vh, seg, base]` (`get`) or `[vh, seg, base, value]` (`set`).
/// `site_kind`: the call site's declared return (`B S C I J F D Z`), or `0`
/// when unknown or for a `set` (r23 wave 2c, lane L8).
pub fn segment_vh_lean_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    write: bool,
    site_kind: u8,
) -> Option<Option<Value>> {
    let (Some(Value::Object(Some(vh))), Some(Value::Object(Some(seg))), Some(Value::Long(base))) = (
        args.first().copied(),
        args.get(1).copied(),
        args.get(2).copied(),
    ) else {
        return None;
    };
    if args.len() != 3 + usize::from(write) {
        return None;
    }
    let value = if write { Some(args[3]) } else { None };
    let (shape, out) = segment_vh_lean(ctx, vh, seg, base, value, site_kind, None)?;
    Some(out.map(|v| box_value_canonical(ctx, v, seg_shape_desc(shape))))
}

/// The funnel's raw-word face of [`segment_vh_lean`] for a `get` (the bits a
/// compiled caller's register takes), or `None` to decline.
pub fn segment_vh_lean_get_bits(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    seg: ObjectRef,
    base: i64,
    site_kind: u8,
    vh_hash: i32,
    seg_class: u32,
) -> Option<i64> {
    if site_kind == 0 {
        return None;
    }
    let keys = Some((vh_hash, ClassId::new(seg_class)));
    match segment_vh_lean(ctx, vh, seg, base, None, site_kind, keys)?.1 {
        Some(Value::Int(v)) => Some(i64::from(v)),
        Some(Value::Long(v)) => Some(v),
        Some(Value::Float(f)) => Some(i64::from(f.to_bits())),
        Some(Value::Double(d)) => Some(d.to_bits() as i64),
        _ => None,
    }
}

/// The funnel's raw-word face of [`segment_vh_lean`] for a `set`; `true` when
/// the store was made.
pub fn segment_vh_lean_set_bits(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    seg: ObjectRef,
    base: i64,
    site_kind: u8,
    value_bits: i64,
    vh_hash: i32,
    seg_class: u32,
) -> bool {
    let value = match site_kind {
        b'B' | b'Z' | b'S' | b'C' | b'I' => Value::Int(value_bits as i32),
        b'J' => Value::Long(value_bits),
        b'F' => Value::Float(f32::from_bits(value_bits as u32)),
        b'D' => Value::Double(f64::from_bits(value_bits as u64)),
        _ => return false,
    };
    let keys = Some((vh_hash, ClassId::new(seg_class)));
    segment_vh_lean(ctx, vh, seg, base, Some(value), site_kind, keys).is_some()
}

/// SBR4 lane segacc: the plan of a real `ValueLayout`'s access handle, filed
/// per thread by `(vm, layout address, ffm epoch, handle-table generation)`.
/// The layout's own `handle` field (`AbstractValueLayout.accessHandle()`'s
/// memo) names the `SegmentVarHandle` whose plan [`seg_vh_plan`] decodes, so a
/// compiled `MemorySegment.get(layout, off)` reaches the same decisions as a
/// `VarHandle.get(segment, off)` without the interface call, the handle
/// fetch and the funnel. A layout whose handle is not built yet (the first
/// access) answers `None` and the JDK's own bytecode builds it.
#[derive(Clone, Copy)]
pub struct SegLayoutFacts {
    pub vm: usize,
    pub addr: usize,
    pub epoch: u64,
    pub generation: u64,
    pub plan: SegVhPlan,
}

pub const SEG_LAYOUT_FACT_WAYS: usize = 8;

thread_local! {
    pub static SEG_LAYOUT_FACTS: std::cell::Cell<[Option<SegLayoutFacts>; SEG_LAYOUT_FACT_WAYS]> =
        const { std::cell::Cell::new([None; SEG_LAYOUT_FACT_WAYS]) };
    pub static SEG_LAYOUT_FACTS_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn seg_layout_plan(ctx: &mut dyn NativeContext, layout: ObjectRef) -> Option<SegVhPlan> {
    let vm = ctx.vm_identity();
    let addr = layout.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let ways = SEG_LAYOUT_FACTS.with(std::cell::Cell::get);
    for way in ways.iter().flatten() {
        if way.addr == addr && way.epoch == epoch && way.vm == vm && way.generation == generation {
            return Some(way.plan);
        }
    }
    let Value::Object(Some(vh)) = real_object_field(
        ctx,
        layout,
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
        "handle",
    ) else {
        return None;
    };
    let plan = seg_vh_plan(ctx, vh)?;
    // `seg_vh_plan` can move the generation (a table change); file under the
    // value the next lookup will read.
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = SEG_LAYOUT_FACTS_NEXT.with(|c| {
        let i = c.get();
        c.set((i + 1) % SEG_LAYOUT_FACT_WAYS);
        i
    });
    let mut updated = ways;
    updated[slot] = Some(SegLayoutFacts {
        vm,
        addr,
        epoch,
        generation,
        plan,
    });
    SEG_LAYOUT_FACTS.with(|c| c.set(updated));
    Some(plan)
}

/// Everything a compiled `MemorySegment.get/set(layout, offset)` door needs to
/// serve one REAL native segment against one real layout WITHOUT a native
/// context: the segment's final fields, the layout's plan, the session's `state`
/// slot (the one thing a close changes, read on every access by the caller) and
/// whether this thread may touch the session (its `owner` is final). Valid for
/// one `(segment address, layout address, ffm epoch)` -- the argument
/// [`SegLeanFacts`] and [`SegLayoutFacts`] rest on.
#[derive(Clone, Copy)]
pub struct SegRealFacts {
    /// The segment's `length`.
    pub size: i64,
    /// The segment's `min`: an arena handle when `handle`, else a plain pointer.
    pub min: i64,
    pub read_only: bool,
    /// Address of the session object (its `state` slot is `state_slot`).
    pub scope: usize,
    pub state_slot: usize,
    /// The session is shared / implicit / global (no `owner`), or owned by the
    /// thread that built these facts (the facts are filed per thread).
    pub owner_ok: bool,
    pub handle: bool,
    /// The access width in bytes.
    pub width: i64,
    /// The layout's alignment (at most `width`).
    pub align: i64,
    pub be: bool,
}

/// [`SegRealFacts`] for `seg` accessed through `layout` at a site whose
/// descriptor names `site_kind` (`B S C I J F D`), or `None` when this pair is
/// not a plain real-native-segment access (a heap segment, a carrier, a
/// layout whose handle is not built yet, a path / variable-offset handle, a
/// layout aligned wider than its size, a read of another carrier): the JDK's
/// own bytecode then serves it and raises whatever it always raised.
pub fn segment_layout_facts(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    seg_class: u32,
    site_kind: u8,
) -> Option<SegRealFacts> {
    if !segment_vh_lean_enabled() || !segment_vh_checks() || !segment_vh_form_enabled() {
        return None;
    }
    let plan = seg_layout_plan(ctx, layout)?;
    let width = seg_shape_width(plan.shape);
    // `getAtIndex` refuses a layout aligned wider than its size, and a
    // non-trivial `vh_offset` is a path handle: neither is a plain access.
    if plan.variable
        || plan.vh_offset != 0
        || plan.enclosing_align > width
        || plan.enclosing_size != width
        || seg_shape_site_kind(plan.shape) != site_kind
    {
        return None;
    }
    let Some(SegLeanFacts {
        size,
        read_only,
        min,
        scope,
        session,
        ..
    }) = seg_lean_facts(ctx, seg, Some(ClassId::new(seg_class)))
    else {
        return segment_layout_heap_facts(ctx, seg, &plan, width);
    };
    // An arena handle goes through the arena store; a plain non-null pointer (a
    // mapped file, an `ofAddress` window) is dereferenced; anything else
    // declines (as `segment_vh_lean_planned` does).
    let handle = crate::panama_libffi::is_arena_handle(min);
    if !handle && (min <= 0 || crate::unsafe_arena_addr_is_tagged(min)) {
        return None;
    }
    let mut owner = [Value::Int(0); 1];
    ctx.get_fields_typed(scope, &[(session.owner, b'L')], &mut owner);
    let owner_ok = match owner[0] {
        Value::Object(None) => true,
        // zs s1/nd: never mint the mirror here (see `segment_vh_lean_planned`);
        // `real_seg_fill` calls this from a JIT helper.
        Value::Object(Some(o)) => ctx.current_thread_object_if_any() == Some(o),
        _ => return None,
    };
    Some(SegRealFacts {
        size,
        min,
        read_only,
        scope: scope.as_ptr() as usize,
        state_slot: session.state,
        owner_ok,
        handle,
        width,
        align: plan.enclosing_align,
        be: plan.be,
    })
}

/// r23 wave 3, lane L8 (`CRATONVM_FFM_SEGMENT_LAYOUT_HEAP_FACTS`, perf,
/// default ON, `=0` kills): [`segment_layout_facts`] for a real HEAP segment
/// (`MemorySegment.ofArray`): the same [`SegRealFacts`] a native segment
/// gets, with `min` the RAW address of the segment's first byte inside its
/// backing array (`array + HEADER_SIZE + start`), so the compiled door's
/// cached road (`vm::jit::helpers::real_seg_serve`) reads and writes it as it
/// reads a native block: one unaligned load or store. Sound for the facts'
/// lifetime -- one `(segment, layout, ffm epoch)`, the epoch bumped at the
/// entry of every collection, the only thing that moves an array (every
/// relocator runs with the mutators parked) -- and byte-exact: a primitive
/// array's payload is its elements little-endian, back to back, which is the
/// JDK's own heap-segment byte order (`panama::heap_bytes_read`'s measured
/// rule). Declines a layout aligned wider than the backing element (the heap
/// segment's `maxAlignMask` refusal, which the JDK raises) and everything
/// [`seg_heap_facts`] declines.
pub fn segment_layout_heap_facts(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    plan: &SegVhPlan,
    width: i64,
) -> Option<SegRealFacts> {
    if !crate::nbflags().ffm_segment_vh_lean_heap
    {
        return None;
    }
    let SegHeapFacts {
        view,
        scope,
        session,
        ..
    } = seg_heap_facts(ctx, seg)?;
    // Cast: an element is 1..=8 bytes.
    if plan.enclosing_align > view.elem_width as i64 {
        return None;
    }
    // Cast: an object address, and the data offset inside it.
    let min = i64::try_from(view.base.as_ptr() as usize)
        .ok()?
        .checked_add(cratonvm_types::HEADER_SIZE as i64)?
        .checked_add(view.start)?;
    if min <= 0 {
        return None;
    }
    let mut owner = [Value::Int(0); 1];
    ctx.get_fields_typed(scope, &[(session.owner, b'L')], &mut owner);
    let owner_ok = match owner[0] {
        Value::Object(None) => true,
        Value::Object(Some(o)) => ctx.current_thread_object_if_any() == Some(o),
        _ => return None,
    };
    Some(SegRealFacts {
        size: view.size(),
        min,
        read_only: view.read_only(),
        scope: scope.as_ptr() as usize,
        state_slot: session.state,
        owner_ok,
        handle: false,
        width,
        align: plan.enclosing_align,
        be: plan.be,
    })
}

/// [`seg_vh_plan`]'s memo, hit only, for a caller that already holds the
/// handle's identity hash (the funnel's receiver memo): no hash re-derivation
/// and no miss line.
pub fn seg_vh_plan_hit(vm: usize, hash: i32, addr: usize) -> Option<SegVhPlan> {
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    SEG_VH_PLAN_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm && line.key == hash && line.addr == addr && line.generation == generation {
            line.value
        } else {
            None
        }
    })
}

pub fn seg_encode_value(value: &Value, width: i64) -> u64 {
    let raw: u64 = match value {
        Value::Long(v) => *v as u64,
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => *v as u32 as u64,
        Value::Float(v) => v.to_bits() as u64,
        _ => 0,
    };
    match width {
        1 => raw & 0xff,
        2 => raw & 0xffff,
        4 => raw & 0xffff_ffff,
        _ => raw,
    }
}

/// Resolve a `MemorySegment` coordinate's raw access point by reading the
/// real implementation object's own fields directly (NOT by calling back
/// into its `unsafeGetBase()`/`unsafeGetOffset()`/`byteSize()` methods — a
/// reentrant native→bytecode call from inside a native invoked by
/// JIT-compiled code crashed here; see the fix commit for the observed
/// fault). Mirrors `sun.misc.Unsafe`'s dual addressing: a null base means
/// `offset` is an absolute native address; a non-null base means `offset` is
/// a byte offset into that (possibly heap/GC-managed) array object.
///
/// Field layout (real JDK `jdk.internal.foreign` classes, JEP 454):
/// `AbstractMemorySegmentImpl.length` (byteSize for every concrete subtype);
/// `NativeMemorySegmentImpl.min` (absolute address; `MappedMemorySegmentImpl`
/// extends it and inherits the field); `HeapMemorySegmentImpl.base`/`.offset`
/// (array object + Unsafe-style byte offset into it).
pub fn segment_raw_access(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh_offset: i64,
    coord_offset: i64,
    width: i64,
) -> Result<(Option<ObjectRef>, i64), MethodCallFailed> {
    const ABSTRACT_SEGMENT: &str = "jdk/internal/foreign/AbstractMemorySegmentImpl";
    const NATIVE_SEGMENT: &str = "jdk/internal/foreign/NativeMemorySegmentImpl";
    const HEAP_SEGMENT: &str = "jdk/internal/foreign/HeapMemorySegmentImpl";

    let class_name = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(seg))
        .unwrap_or_else(|| Arc::from(""));
    // Round 12 wave 7 (lane ffm4): a CratonVM-minted carrier
    // (`cratonvm/internal/foreign/MemorySegmentImpl`) has none of the JDK
    // fields below -- resolving `NativeMemorySegmentImpl.min` or
    // `AbstractMemorySegmentImpl.length` against the JDK class and reading
    // that slot index off the carrier read its `readOnly` flag as the address
    // and its pointer as the size. It goes through the readers that know both
    // models, as the layout VarHandles do.
    if segment_vh_checks() && !class_name.starts_with("jdk/internal/foreign/") {
        let size = crate::panama_libffi::segment_byte_size(ctx, seg);
        let (base, base_off) = match crate::panama::heap_segment_view(ctx, seg) {
            Some(view) => (Some(view.base), view.start),
            None => (None, crate::panama_libffi::segment_address(ctx, seg)),
        };
        return segment_raw_bounds(base, base_off, size, vh_offset, coord_offset, width)
            .ok_or_else(|| segment_raw_refusal(ctx, seg, coord_offset, width));
    }
    let (base, base_off) = if class_name.contains("Heap") {
        // `HeapMemorySegmentImpl.offset` is `Unsafe`-style and must be
        // un-biased before use — see `panama_libffi::heap_segment_byte_start`,
        // the one shared reader for this rule (NOM F35-2; it used to be a
        // second, independent `const ABASE: i64 = 16` here). Skipping it
        // lands every heap-segment access 16 bytes past its intended target
        // (silently corrupting data when still in-bounds, throwing when it
        // overflows the backing array).
        let base_idx = ctx.resolve_field_index(HEAP_SEGMENT, "base");
        let off_idx = ctx.resolve_field_index(HEAP_SEGMENT, "offset");
        let base = match (base_idx, base_idx.map(|i| ctx.get_field(seg, i))) {
            (Some(_), Some(Value::Object(b))) => b,
            _ => None,
        };
        let off = match off_idx {
            Some(i) => match ctx.get_field(seg, i) {
                Value::Long(n) => crate::panama_libffi::heap_segment_byte_start(n),
                _ => 0,
            },
            None => 0,        };
        (base, off)    } else {
        let off = match ctx.resolve_field_index(NATIVE_SEGMENT, "min") {
            Some(i) => match ctx.get_field(seg, i) {
                Value::Long(n) => n,
                _ => 0,
            },
            None => 0,        };
        (None, off)    };
    let size = match ctx.resolve_field_index(ABSTRACT_SEGMENT, "length") {
        Some(i) => match ctx.get_field(seg, i) {
            Value::Long(n) => n,
            _ => 0,
        },
        None => 0,
    };
    segment_raw_bounds(base, base_off, size, vh_offset, coord_offset, width)
        .ok_or_else(|| segment_raw_refusal(ctx, seg, coord_offset, width))
}

/// `CRATONVM_FFM_SEGMENT_VH_CHECKS` (default on): a real `SegmentVarHandle`
/// access checks what the JDK's `checkSegment` / `offset` and the session
/// check do -- read-only (`IllegalArgumentException`), bounds
/// (`IndexOutOfBoundsException`, was `IllegalStateException`), the absolute
/// address against the layout's alignment on a native segment
/// (`IllegalArgumentException: Misaligned access`), and the scope, under an
/// access window -- and decodes a CratonVM carrier through the readers that
/// know its slots. Off restores round 12 wave 6's unchecked access (round 12
/// wave 7, lane ffm4).
pub fn segment_vh_checks() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Bounds-check a `SegmentVarHandle` access of `width` bytes at `vh_offset +
/// coord_offset` in a segment of `size` bytes whose data starts at `base_off`
/// (an absolute address, or a byte offset into `base`'s data), and answer
/// `(base, address)`, or `None` when it does not fit ([`segment_raw_refusal`]).
pub fn segment_raw_bounds(
    base: Option<ObjectRef>,
    base_off: i64,
    size: i64,
    vh_offset: i64,
    coord_offset: i64,
    width: i64,
) -> Option<(Option<ObjectRef>, i64)> {
    let total_offset = vh_offset.checked_add(coord_offset)?;
    let end = total_offset.checked_add(width);
    if total_offset < 0 || end.map_or(true, |e| e > size) {
        return None;
    }
    let addr = base_off.checked_add(total_offset)?;
    Some((base, addr))
}

/// [`segment_raw_bounds`]' refusal: `outOfBoundException(coord_offset,
/// width)`'s text naming the segment (j27 wave 4, lane FFMMSG: it said
/// `offset=.., width=.., size=..`). HotSpot's length is the enclosing
/// layout's size, which is `width` for a value-layout handle (`vh_offset` 0).
/// Builds the text without running Java: the caller holds an access window.
pub fn segment_raw_refusal(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    coord_offset: i64,
    width: i64,
) -> MethodCallFailed {
    let segment = crate::panama::segment_to_string(ctx, seg);
    let message = crate::panama::out_of_bounds_message(&segment, coord_offset, width);
    if segment_vh_checks() {
        // The JDK's class for an access past the end (`checkBounds`).
        MethodCallFailed::from(RuntimeError::IndexOutOfBoundsException {
            message: Some(message),
        })
    } else {
        MethodCallFailed::from(RuntimeError::IllegalStateException { message })
    }
}

/// The scope and alignment half of a `SegmentVarHandle` access (round 12 wave
/// 7, lane ffm4; `r12w6-ffm3-segment-varhandle-has-no-scope-check`). The
/// caller holds an `ffm_fast::AccessWindow` from before this to after its
/// last touch of the block, so a shared close either waits for the access or
/// is what the scope check reads. `seg` is forwarded: a real session's check
/// can run Java.
pub fn segment_vh_check_scope(
    ctx: &mut dyn NativeContext,
    seg: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    if !segment_vh_checks() {
        return Ok(());
    }
    crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, seg)
}

/// Is a native access at absolute `addr` misaligned for `align` -- the rule
/// `isAlignedForElement` applies (a native segment's `maxAlignMask` is 0)? A
/// synthetic `Unsafe` arena handle is not an address and is not checked;
/// neither is a heap segment (its rule involves the array's element
/// alignment and is left to the bounds check). The refusal is
/// `panama::native_misaligned_refusal` (j27 wave 4, lane FFMMSG: HotSpot's
/// `checkEnclosingLayout` text; it said `Misaligned access at address: N`).
pub fn segment_vh_misaligned(align: i64, base: Option<ObjectRef>, addr: i64) -> bool {
    segment_vh_checks()
        && base.is_none()
        && !crate::unsafe_arena_contains(addr)
        && crate::panama::native_misaligned(addr as usize, align.max(1) as u64)
}

/// j27 wave 6 (lane T1LOOPFFM): the legacy `segment_vh_get` / `_set` road's
/// `SegmentVarHandle.checkSegment` -> `checkEnclosingLayout(base, enclosing)`,
/// run as JDK 25 runs it: after the read-only test, BEFORE the session check
/// and before the access window opens (its refusals can run Java). So, as on
/// HotSpot, the WHOLE enclosing layout must fit at `base` (a member handle
/// was bounded by its own field, and named its numbers), `base` must meet the
/// enclosing alignment (a member handle tested `base + offset`), a closed
/// session does not hide either refusal, and the misaligned text carries its
/// `(of ..)` clause (`enclosing` is still the reference read at entry). The
/// one checker the form road uses, [`segment_vh_check_enclosing`]. Asked
/// under `CRATONVM_FFM_BOUNDS_BEFORE_SCOPE` (`panama::ffm_bounds_before_scope`,
/// HotSpot's order, default ON); off, or with an unreadable layout size, the
/// road keeps its checks after the scope, as before.
pub fn segment_vh_enclosing_before_scope(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh: ObjectRef,
    enclosing: ObjectRef,
    align: i64,
    base: i64,
) -> Result<(), MethodCallFailed> {
    if !segment_vh_checks() || !crate::panama::ffm_bounds_before_scope() {
        return Ok(());
    }
    let Ok(size) = crate::panama_libffi::layout_total_size(ctx, enclosing) else {
        return Ok(());
    };
    // Cast: a layout's byte size is a Java `long`.
    segment_vh_check_enclosing(ctx, seg, vh, size as i64, align, base)
}

/// Heap-segment access for `SegmentVarHandle`. `addr` is the Unsafe-style
/// absolute byte offset into `base` as returned by `unsafeGetOffset()`; real
/// `byte[]` segments carry `ARRAY_BYTE_BASE_OFFSET` baked into that value so
/// it lands directly on `base`'s own element indices.
///
/// NOM F35-3 (was a real defect, not just a comment): this used to read `base`
/// one BYTE per `get_array_element` call regardless of its actual element
/// type, which is right only for `byte[]`. A `VarHandle` over an
/// `int[]`-backed `MemorySegment` (`MemorySegment.ofArray(new int[8])`) read
/// element `addr..addr+width` of a `byte[]` view that does not exist, i.e. it
/// read INT elements at BYTE granularity — wrong data, not a crash, so nothing
/// short of comparing against the oracle would show it. `heap_bytes_read` is
/// `panama.rs`'s own general reader (`heap_segment_read`'s core), shared here
/// rather than re-derived, per that file's own doc comment asking for exactly
/// this ("make those two `pub` ... or move both into one module").
pub fn segment_heap_get(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    addr: i64,
    width: i64,
) -> Option<u64> {
    let elem_type = ctx.heap_element_type_of(base);
    let elem_width = crate::panama::heap_element_width(elem_type)? as i64;
    let byte_len = (ctx.array_length(base) as i64).checked_mul(elem_width)?;
    if addr < 0 || addr.checked_add(width)? > byte_len {
        return None;
    }
    // JIT round 20 wave 8 (lane huntFFM): one access on the flat payload.
    if let Some(raw) = segment_heap_payload_read(ctx, base, addr as usize, width as usize) {
        return Some(raw);
    }
    Some(crate::panama::heap_bytes_read(
        ctx,
        base,
        elem_width as usize,
        addr,
        width as usize,
    ))
}

/// JIT round 20 wave 8 (lane huntFFM): a heap-segment access is ONE access of
/// exactly its own bytes, as JDK 25's `Unsafe` access on the base array is.
/// The element-wise fallback ([`crate::panama::heap_bytes_read`] /
/// [`crate::panama::heap_bytes_write`]) reads, and for a store
/// read-modify-writes, the whole ELEMENT once per byte: a `JAVA_BYTE` /
/// `JAVA_SHORT` store into an `int[]` / `long[]` segment rewrote its
/// neighbours' bytes (a concurrent store to another byte of the same element
/// was lost), and an aligned `JAVA_INT` / `JAVA_LONG` read or store was torn
/// into byte steps where the JDK promises single-copy atomicity for an aligned
/// plain access (probe `R20HuntFFMHeapTear`: HotSpot `lost.* 0`, `torn.* 0`).
///
/// An aligned 2/4/8-byte access is one hardware atomic on the payload
/// (`NativeContext::primitive_array_atomic_update`, `f` answering `None`: a
/// load); anything else one byte copy of exactly `width` bytes
/// (`read_primitive_array_bytes`). `None`: no flat payload (the caller's
/// element-wise reading stands). `off` is the payload byte offset the caller
/// bounds-checked.
pub fn segment_heap_payload_read(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    off: usize,
    width: usize,
) -> Option<u64> {
    if width == 0 || width > 8 {
        return None;
    }
    if matches!(width, 2 | 4 | 8) && off % width == 0 {
        if let Some((cur, _)) = ctx.primitive_array_atomic_update(base, off, width, &|_| None) {
            return Some(cur);
        }
    }
    let mut b = [0u8; 8];
    (ctx.read_primitive_array_bytes(base, off, &mut b[..width]) == width)
        .then(|| u64::from_le_bytes(b))
}

/// The store half of [`segment_heap_payload_read`]: the low `width` bytes of
/// `raw` (little-endian, the payload's order) as one access. `false`: no flat
/// payload, nothing written (the caller's element-wise store stands).
pub fn segment_heap_payload_write(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    off: usize,
    width: usize,
    raw: u64,
) -> bool {
    if width == 0 || width > 8 {
        return false;
    }
    let raw = if width == 8 {
        raw
    } else {
        raw & ((1u64 << (width * 8)) - 1)
    };
    if matches!(width, 2 | 4 | 8)
        && off % width == 0
        && ctx
            .primitive_array_atomic_update(base, off, width, &|_| Some(raw))
            .is_some()
    {
        return true;
    }
    ctx.write_primitive_array_bytes(base, off, &raw.to_le_bytes()[..width])
}

/// Write-side twin of [`segment_heap_get`] — see its doc comment (NOM F35-3).
pub fn segment_heap_set(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    addr: i64,
    width: i64,
    raw: u64,
) -> bool {
    let elem_type = ctx.heap_element_type_of(base);
    let Some(elem_width) = crate::panama::heap_element_width(elem_type) else {
        return false;
    };
    let elem_width = elem_width as i64;
    let Some(byte_len) = (ctx.array_length(base) as i64).checked_mul(elem_width) else {
        return false;
    };
    match addr.checked_add(width) {
        Some(end) if addr >= 0 && end <= byte_len => {}
        _ => return false,
    }
    // JIT round 20 wave 8 (lane huntFFM): see `segment_heap_payload_read`.
    if segment_heap_payload_write(ctx, base, addr as usize, width as usize, raw) {
        return true;
    }
    crate::panama::heap_bytes_write(
        ctx,
        base,
        elem_type,
        elem_width as usize,
        addr,
        width as usize,
        raw,
    );
    true
}

/// Read `width` bytes at a null-base (off-heap) `MemorySegment` address.
///
/// `addr` is NOT necessarily a real OS pointer: `Unsafe.allocateMemory` (which
/// real `Arena.ofConfined()/allocate()` bytecode calls into) hands out
/// synthetic tagged arena handles (see `unsafe_arena_contains` doc), not real
/// pointers — dereferencing one directly segfaults. Real `MMapDirectory`
/// segments (backed by an actual OS mapping) DO carry a real pointer, so both
/// cases must be handled, exactly mirroring `vm_exec.rs`'s
/// `copy_from_native_memory`.
pub fn segment_native_get(addr: i64, width: i64) -> Result<u64, MethodCallFailed> {
    if crate::unsafe_arena_contains(addr) {
        let mut buf = [0u8; 8];
        if !crate::unsafe_arena_copy_out(addr, &mut buf[..width as usize]) {
            return Err(RuntimeError::IllegalStateException {
                message: "Out of bound access on off-heap MemorySegment arena handle".into(),
            }
            .into());
        }
        return Ok(u64::from_le_bytes(buf));
    }
    let ptr = addr as usize as *const u8;
    if ptr.is_null() {
        return Err(RuntimeError::IllegalStateException {
            message: "MemorySegment access via null address".into(),
        }
        .into());
    }
    // SAFETY: bounds-checked against the segment's declared size and
    // overflow-checked address arithmetic in `segment_raw_access`; not a
    // synthetic arena handle (checked above), so this is a real OS pointer.
    // Unaligned reads (round 12 wave 7, lane ffm4): an `*_UNALIGNED` layout
    // may address any byte, and a plain dereference there is undefined
    // behaviour in Rust.
    Ok(unsafe {
        match width {
            1 => *ptr as u64,
            2 => (ptr as *const u16).read_unaligned() as u64,
            4 => (ptr as *const u32).read_unaligned() as u64,
            _ => (ptr as *const u64).read_unaligned(),
        }
    })
}

/// Write `width` bytes at a null-base (off-heap) `MemorySegment` address —
/// see `segment_native_get`.
pub fn segment_native_set(addr: i64, width: i64, raw: u64) -> Result<(), MethodCallFailed> {
    if crate::unsafe_arena_contains(addr) {
        let bytes = raw.to_le_bytes();
        if !crate::unsafe_arena_copy_in(addr, &bytes[..width as usize]) {
            return Err(RuntimeError::IllegalStateException {
                message: "Out of bound access on off-heap MemorySegment arena handle".into(),
            }
            .into());
        }
        return Ok(());
    }
    let ptr = addr as usize as *mut u8;
    if ptr.is_null() {
        return Err(RuntimeError::IllegalStateException {
            message: "MemorySegment access via null address".into(),
        }
        .into());
    }
    // SAFETY: see `segment_native_get`, unaligned writes included.
    unsafe {
        match width {
            1 => *ptr = raw as u8,
            2 => (ptr as *mut u16).write_unaligned(raw as u16),
            4 => (ptr as *mut u32).write_unaligned(raw as u32),
            _ => (ptr as *mut u64).write_unaligned(raw),
        }
    }
    Ok(())
}

/// `SegmentVarHandle.get(segment, offset)` — handled inline by `varhandle_get`
/// once `is_segment_var_handle` matches. Returns `None` (fall through to the
/// existing dispatch) only if the VarHandle's own fields can't be resolved;
/// otherwise always produces a value or an error.
pub fn segment_vh_get(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> Option<MethodCallResult> {
    if segment_vh_checks() && segment_vh_form_enabled() {
        if let Some(plan) = seg_vh_plan(ctx, this) {
            // `checkSegment`'s `Objects.requireNonNull`.
            let Some(Value::Object(Some(seg))) = args.get(1).copied() else {
                return Some(Err(
                    RuntimeError::NullPointerException { message: None }.into()
                ));
            };
            return Some(segment_vh_form_access(ctx, plan, this, seg, args, false));
        }
    }
    let (enclosing, vh_offset, be) = segment_vh_fields(ctx, this)?;
    let seg = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Some(Ok(Some(Value::Object(None)))),
    };
    let coord_offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    };
    let shape = segment_layout_shape(ctx, enclosing);
    let width = seg_shape_width(shape);
    // Read before the scope check, which can run Java and move `enclosing`.
    let align = crate::panama_libffi::layout_align(ctx, enclosing) as i64;
    Some((|| -> MethodCallResult {
        // Round 12 wave 7 (lane ffm4): an access window from before the scope
        // check to after the read, dropped before the boxing allocation.
        // j27 wave 6 (lane T1LOOPFFM): `checkEnclosingLayout` first, as JDK 25.
        segment_vh_enclosing_before_scope(ctx, seg, this, enclosing, align, coord_offset)?;
        let window = crate::ffm_fast::AccessWindow::open();
        let mut seg = seg;
        segment_vh_check_scope(ctx, &mut seg)?;
        let (base, addr) = segment_raw_access(ctx, seg, vh_offset, coord_offset, width)?;
        // `isAlignedForElement(base, enclosing)`: the enclosing layout's
        // start, `addr` less the handle's own offset (j27 wave 6: a member
        // handle tested its field's address against the enclosing alignment).
        if segment_vh_misaligned(align, base, addr.wrapping_sub(vh_offset)) {
            // j27 wave 4 (lane FFMMSG): HotSpot's text, after the window
            // closes. `enclosing` may have moved in the scope check: no
            // `(of ..)` clause.
            drop(window);
            return Err(crate::panama::native_misaligned_refusal(
                ctx,
                seg,
                None,
                coord_offset,
                align,
            ));
        }
        let raw = match base {
            None => segment_native_get(addr, width)?,
            Some(base_obj) => segment_heap_get(ctx, base_obj, addr, width).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::IllegalStateException {
                    message: "Out of bound access on heap MemorySegment".into(),
                })
            })?,
        };
        drop(window);
        let raw = if be { seg_swap_bytes(raw, width) } else { raw };
        // CANONICAL — same measurement as `layout_vh_get`: an FFM
        // `MemorySegment` read handed back through a `VarHandle` is
        // `X.valueOf`-boxed on HotSpot. `seg_decode_value` and
        // `seg_shape_desc` switch on the same `SegShape`, so the variant and
        // the descriptor agree (including `Address`, which is `Value::Long`
        // and `"J"` in both).
        Ok(Some(box_value_canonical(
            ctx,
            seg_decode_value(shape, raw),
            seg_shape_desc(shape),
        )))
    })())
}

/// `SegmentVarHandle.set(segment, offset, value)` or path-bound
/// `SegmentVarHandle.set(segment, value)` - see `segment_vh_get`.
pub fn segment_vh_set(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> Option<MethodCallResult> {
    if segment_vh_checks() && segment_vh_form_enabled() {
        if let Some(plan) = seg_vh_plan(ctx, this) {
            // `checkSegment`'s `Objects.requireNonNull`.
            let Some(Value::Object(Some(seg))) = args.get(1).copied() else {
                return Some(Err(
                    RuntimeError::NullPointerException { message: None }.into()
                ));
            };
            return Some(segment_vh_form_access(ctx, plan, this, seg, args, true));
        }
    }
    let (enclosing, vh_offset, be) = segment_vh_fields(ctx, this)?;
    let seg = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Some(Ok(None)),
    };
    let (coord_offset, value_index) = if args.len() >= 4 {
        (
            match args.get(2) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n)) => *n as i64,
                _ => 0,
            },
            3,
        )
    } else {
        (0, 2)
    };
    let value = args.get(value_index).cloned().unwrap_or(Value::Int(0));
    let shape = segment_layout_shape(ctx, enclosing);
    let width = seg_shape_width(shape);
    // Read before the scope check, which can run Java and move `enclosing`.
    let align = crate::panama_libffi::layout_align(ctx, enclosing) as i64;
    Some((|| -> MethodCallResult {
        // Direct field read (not `isReadOnly()` invoke_virtual) — see
        // `segment_raw_access`'s doc comment on avoiding reentrant native→
        // bytecode calls from inside a native invoked by JIT-compiled code.
        if segment_vh_checks() {
            // Round 12 wave 7 (lane ffm4): the reader that knows both segment
            // models, and the JDK's exception (`checkAccess`).
            let read_only = crate::phases_late::foreign_ffm::p67_segment_is_read_only(
                ctx,
                &[Value::Object(Some(seg))],
            )?;
            if matches!(read_only, Some(Value::Int(n)) if n != 0) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Attempt to write a read-only segment".into(),
                }
                .into());
            }
        } else {
            let is_ro = match ctx
                .resolve_field_index("jdk/internal/foreign/AbstractMemorySegmentImpl", "readOnly")
            {
                Some(i) => matches!(ctx.get_field(seg, i), Value::Int(n) if n != 0),
                None => false,
            };
            if is_ro {
                return Err(RuntimeError::IllegalStateException {
                    message: "Attempted write on read-only MemorySegment".into(),
                }
                .into());
            }
        }
        // Encoded before the scope check can run Java: an `ADDRESS` value is a
        // `MemorySegment` reference, stored as its address (it used to encode
        // as 0); a null one is `NullPointerException`, as on HotSpot.
        let mut raw = match (shape, value) {
            (SegShape::Address, Value::Object(Some(target))) if segment_vh_checks() => {
                crate::panama_libffi::segment_address(ctx, target) as u64
            }
            (SegShape::Address, Value::Object(None)) if segment_vh_checks() => {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            _ => seg_encode_value(&value, width),
        };
        if be {
            raw = seg_swap_bytes(raw, width);
        }
        // As in `segment_vh_get` (j27 wave 6, lane T1LOOPFFM): after the
        // read-only test above, before the session.
        segment_vh_enclosing_before_scope(ctx, seg, this, enclosing, align, coord_offset)?;
        // Round 12 wave 7 (lane ffm4): an access window from before the scope
        // check to after the write.
        let _window = crate::ffm_fast::AccessWindow::open();
        let mut seg = seg;
        segment_vh_check_scope(ctx, &mut seg)?;
        let (base, addr) = segment_raw_access(ctx, seg, vh_offset, coord_offset, width)?;
        // The enclosing layout's start, as in `segment_vh_get`.
        if segment_vh_misaligned(align, base, addr.wrapping_sub(vh_offset)) {
            // As in `segment_vh_get` (j27 wave 4, lane FFMMSG).
            drop(_window);
            return Err(crate::panama::native_misaligned_refusal(
                ctx,
                seg,
                None,
                coord_offset,
                align,
            ));
        }
        match base {
            None => segment_native_set(addr, width, raw)?,
            Some(base_obj) => {
                if !segment_heap_set(ctx, base_obj, addr, width, raw) {
                    return Err(RuntimeError::IllegalStateException {
                        message: "Out of bound access on heap MemorySegment".into(),
                    }
                    .into());
                }
            }
        }
        Ok(None)
    })())
}


/// The descriptor of the variable a `VarHandle` access reads or writes, for the
/// call shape in `args`.
///
/// It answers the same question the access natives themselves answer when they
/// pick a branch, in the SAME order, so the descriptor cannot disagree with the
/// value the native produced:
///
/// 1. `vh_array_call` matches → the array's element type. Every RMW native
///    probes this first (see `varhandle_get_and_set` / `_add` / `_bitwise` /
///    `compare_and_exchange`), including for byte-array-view handles, which
///    those natives do not special-case.
/// 2. otherwise the handle's own `field_desc`, via the meta side table
///    (`vh_type_desc`), which is what the instance/static branches read.
///
/// Falls back to `DESC_REF` when nothing resolves. That is deliberate and
/// lossless: `box_value`'s catch-all arm returns the value untouched for a
/// reference descriptor, so an unresolvable handle keeps exactly today's
/// behaviour instead of guessing a wrapper class.
pub fn vh_access_value_desc(ctx: &mut dyn NativeContext, args: &[Value]) -> Cow<'static, str> {
    if let Some((arr, _)) = vh_array_call(ctx, args) {
        return Cow::Borrowed(array_element_desc(ctx, arr));
    }
    match args.first() {
        Some(Value::Object(Some(this))) => vh_type_desc(ctx, *this),
        _ => Cow::Borrowed(DESC_REF),
    }
}

/// THE boxing funnel for every `VarHandle` access mode that HANDS BACK the
/// accessed variable.
///
/// # The defect this closes
///
/// Every such mode is registered under the erased
/// `([Ljava/lang/Object;)Ljava/lang/Object;` descriptor, so the native's own
/// contract is "return a reference". `varhandle_get` honoured it (it calls
/// `box_value`); `getAndSet`, `getAndAdd`, `getAndBitwise*` and
/// `compareAndExchange` returned the BARE primitive `Value`. Nothing
/// downstream repairs that — `vm_exec.rs`'s `unbox_poly_return` passes
/// `b'L' | b'['` through untouched — so a call site with no cast, whose
/// descriptor therefore ends in `)Ljava/lang/Object;`, received a raw `Int`
/// into a reference return slot. Measured: `System.out.println(
/// va.getAndSet(arr, 1, 20))` printed `null` where HotSpot 25.0.3 prints `2`.
/// The cast form `(int) va.getAndSet(...)` was correct only because its
/// descriptor is then `([III)I` and the coercion path accepts a raw `Int`.
///
/// # Why the boxing has to be HERE and not at the poly-return boundary
///
/// `Value::Int` carries `boolean`, `byte`, `char`, `short` AND `int` (see
/// `cratonvm_types::Value`), so the value's tag does not name its wrapper.
/// Measured on the JDK 25 oracle, HotSpot boxes by the VARIABLE's type:
/// `boolean[]` → `java.lang.Boolean` (prints `true`), `int[]` →
/// `java.lang.Integer`. `unbox_poly_return` sees only the value and the
/// call-site descriptor, and an erased call site says nothing but "Object" —
/// boxing there would have to guess `Integer` and would print `1` for a
/// `boolean[]`. The native is the only layer that knows the element/field
/// descriptor, so this is the only layer that can be right.
///
/// # Idempotent by construction
///
/// A value that is already `Value::Object(_)` passes straight through. That
/// covers `null`, a reference-typed variable, and `varhandle_get`'s own inline
/// `box_value` calls, so routing a mode through this funnel can never
/// double-box. It is also why `varhandle_get` keeps its inline boxing: the two
/// cannot fight.
///
/// # Relationship to `varhandle_reference_return_mismatch` (vm_exec.rs)
///
/// That check fires when a BOXED primitive reaches a call site whose declared
/// reference return type the wrapper is not assignable to — i.e. it can only
/// ever see values this funnel produces, and it accepts `java/lang/Object` and
/// every wrapper supertype. So the two agree by construction: this funnel makes
/// `Object o = vh.getAndSet(...)` correct (the check declines — `Object` is a
/// supertype), and makes `String s = (String) vh.getAndSet(...)` reach the
/// check that HotSpot answers with `WrongMethodTypeException`. Before this fix
/// the raw `Int` was not an object at all, so the check silently could not fire
/// on the RMW modes; there is no shape where both act.
pub fn vh_box_access_result(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    result: MethodCallResult,
) -> MethodCallResult {
    let Some(value) = result? else {
        return Ok(None);
    };
    if matches!(value, Value::Object(_)) {
        return Ok(Some(value));
    }
    let desc = vh_access_value_desc(ctx, args);
    // Widen BEFORE boxing, exactly as `varhandle_get`'s three field arms and
    // `Field.get` do. This `let` SHADOWS the binding twelve lines up, after
    // the `matches!(value, Value::Object(_))` early return has already
    // excluded references — so the coercion only ever sees a primitive, which
    // is its contract.
    let value = crate::lang_class::coerce_reflective_field_value(value, &desc);
    // CANONICAL — measured `vh.getAndSetInt` = true and
    // `vh.compareAndExchangeInt` = true: the value a read-modify-write mode
    // HANDS BACK is `X.valueOf`-boxed on HotSpot, exactly like a plain `get`.
    //
    // The variant guard inside `box_value_canonical` matters HERE more than
    // anywhere else in this file: `desc` comes from `vh_access_value_desc`,
    // which resolves the VARIABLE's declared descriptor, while `value` came
    // from whatever the access mode computed. A `("J", Value::Int)` pair is
    // therefore reachable, and the helper answers it by falling back to
    // `box_value` — i.e. today's behaviour verbatim, never a cached
    // `Long.valueOf(0)`. `vh_access_value_desc`'s own `DESC_REF` fallback is
    // likewise untouched: a reference descriptor is not one of the six arms.
    //
    // F19-1 N3 scoped the missing `coerce_reflective_field_value` to
    // `varhandle_get`'s three field arms. MEASURED (`VhLong.java`),
    // `vh.getAndSetLong` hands back the old value **5** as a canonical
    // `Long` — so this funnel is a FOURTH member of that set, not a bystander:
    // the same `("J", Value::Int(5))` slot reaches it through `getAndSet` and
    // came out as a `Long` wrapper carrying compact-Int bits. The coercion
    // above closes it, and it is the member a fix that reads only
    // `varhandle_get` leaves behind — three of four looks complete.
    Ok(Some(box_value_canonical(ctx, value, &desc)))
}

/// VarHandle.get(receiver) → value
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver] for instance fields.
/// `CRATONVM_VH_NULL_COORDINATE_NPE=0` — restore the pre-2026-09-02 behaviour,
/// in which a `VarHandle` access with a NULL coordinate answered instead of
/// throwing.
///
/// Default on. The switch exists because this turns silence into an exception
/// on a path any workload can reach, so a suite that starts failing has to be
/// bisectable to this and not to a rebuild.
pub fn vh_null_coordinate_npe_enabled() -> bool {
    !matches!(
        None::<String>.as_deref(),
        Some("0")
    )
}

/// The null check every access mode owes its leading COORDINATE.
///
/// # What was wrong
///
/// HotSpot raises `NullPointerException` when a `VarHandle` access is given a
/// null coordinate — the receiver of an instance-field handle, or the array of
/// an array-element handle. CratonVM answered instead. Measured on JDK 25 with
/// `RJdkVarHandleNullCoord`, which walks every access mode against an `int`
/// instance field, an `Object` instance field and an `int[]` element handle:
/// **75 of its 76 rows disagreed**, and the one that agreed is the control (a
/// STATIC-field handle, which has no coordinate to be null).
///
/// ```text
///   35 rows  returned 0        primitive reads, compareAndExchange, getAndAdd, bitwise
///   15 rows  returned false    every CAS mode
///   13 rows  returned normally every write mode  <- a lost store, silently
///   12 rows  returned null     reference reads
///    1 row   correct           the static-handle control
/// ```
///
/// The write rows are the worst of them: a store through a null receiver was
/// simply dropped, so the next read returned a stale value that looks
/// legitimate. The CAS rows are next: answering `false` tells the caller
/// "somebody else won the race", which is a retry loop rather than a failure.
///
/// # Why it is one check and not thirty
///
/// The registry is FIRST-WINS and `--dump-native-registry` names this file for
/// all 37 `java/lang/invoke/VarHandle` registrations — `phases_late/
/// reflect_invoke.rs` registers many of the same names and never owns a slot.
/// Those 37 names reach seven registered entry points, and this is called from
/// each of them.
///
/// # Why it is free
///
/// `VarHandle.get` is 21 368 822 calls on one netty phase, so a check that
/// costs anything per access is a regression. Only a leading coordinate that
/// is ACTUALLY NULL (or absent) reaches the slow path below; everything else —
/// a live object, or a primitive, which is what `args[1]` is for a
/// static-field handle's `set` — returns after one `match`.
///
/// # What it deliberately does not cover
///
/// `SegmentVarHandle` and the `MemorySegment` layout kind are skipped: their
/// null behaviour was not measured against the oracle, and a guard that
/// guesses is worse than none. `RJdkVarHandleNullCoord` covers what is
/// asserted here and nothing else.
pub fn vh_check_leading_coordinate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &str,
) -> Result<(), MethodCallFailed> {
    vh_check_leading_coordinate_row(ctx, args, &None, mode)
}

/// [`vh_check_leading_coordinate`] over the row the prologue read (wave 47,
/// [`VARHANDLE_ONE_ROW_READ`]). A STATIC row has no coordinate: its `get`
/// has no `args[1]` at all, so every static read took the slow path below
/// (a flag-table lookup, a class-name fetch and a row read) only to return.
pub fn vh_check_leading_coordinate_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    // The access mode (`get`, `setVolatile`, ...): an array handle's message
    // depends on it ([`vh_null_array_message`]).
    mode: &str,
) -> Result<(), MethodCallFailed> {
    // The hot path. A present, non-null leading argument is every ordinary
    // access; a primitive there belongs to a static-field handle, which has no
    // coordinate at all.
    match args.get(1) {
        Some(Value::Object(None)) | None => {}
        _ => return Ok(()),
    }
    if matches!(row, Some(Some(m)) if m.kind == VH_KIND_STATIC) {
        return Ok(());
    }
    if !vh_null_coordinate_npe_enabled() {
        return Ok(());
    }
    let Some(this) = args.first().and_then(|v| match v {
        Value::Object(Some(o)) => Some(*o),
        _ => None,
    }) else {
        // No handle to ask. Leave it to the access mode's own decode, which is
        // what produced today's answer for this shape.
        return Ok(());
    };
    if is_segment_var_handle(ctx, this) {
        return Ok(());
    }
    // The kind, resolved the same way `vh_value_and_coordinate_descriptors`
    // resolves it, and for the same reason: a REAL-JDK array handle has no
    // side-table entry and its slot 0 is the real `vform` REFERENCE, so
    // reading it as a kind tag would silently call it an instance handle.
    let meta_kind = vh_meta_get(ctx, this).as_deref().map(|m| m.kind);
    let kind = match meta_kind {
        Some(k) => k,
        None => {
            if real_array_var_handle_descriptors(ctx, this).is_some() {
                VH_KIND_ARRAY
            } else {
                ctx.get_field(this, VH_KIND)
                    .as_int()
                    .unwrap_or(VH_KIND_INSTANCE)
            }
        }
    };
    let coordinate = match kind {
        // No coordinate: `args[1]` is the VALUE, and a null one is a legal
        // store of null into a reference static.
        VH_KIND_STATIC => return Ok(()),
        // HotSpot's own texts (interpreter round i1 wave 52, lane L4; probe
        // `L4W52VarHandleNullMessages`, measured on 25.0.3 over every mode):
        // an instance handle's `Objects.requireNonNull` has none, and an
        // array handle's is the helpful NPE of its `VarHandle<T>s$Array` body
        // ([`vh_null_array_message`]).
        VH_KIND_INSTANCE => {
            // j27 wave 9 (lane W9EXC): the JDK frames HotSpot lists, with
            // `Objects.requireNonNull` on top.
            let (_, value_desc, _, _) = vh_value_and_coordinate_descriptors(ctx, this);
            let variable = value_desc.as_bytes().first().copied().unwrap_or(0);
            if let Some(chain) = vh_null_holder_chain(mode, variable) {
                let _ = ctx.note_served_raise_chain(chain);
            }
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        VH_KIND_ARRAY => {
            let (_, value_desc, _, _) = vh_value_and_coordinate_descriptors(ctx, this);
            return Err(RuntimeError::NullPointerException {
                message: Some(vh_null_array_message(mode, &value_desc)),
            }
            .into());
        }
        // The views (interpreter round i2 wave 53, lane L4; probe
        // `L4W53VarHandleRefusalTexts`, measured on 25.0.3): a byte-array
        // view's `get` / `set` (its only modes; the others are refused
        // first) is the helpful NPE of `VarHandleByteArrayAs<T>s.offset`,
        // which reads `ba.length`; a `ByteBuffer` view's every mode is
        // `Objects.requireNonNull`, with no message.
        VH_KIND_BYTE_VIEW_LE | VH_KIND_BYTE_VIEW_BE => {
            Some("Cannot read the array length because \"ba\" is null")
        }
        VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE => None,
        // An unmeasured kind (the `MemorySegment` layout handle) keeps today's
        // behaviour rather than inheriting a rule nothing checked.
        _ => return Ok(()),
    };
    Err(RuntimeError::NullPointerException {
        message: coordinate.map(str::to_string),
    }
    .into())
}

/// The helpful `NullPointerException` text of an array-element `VarHandle`
/// access mode `mode` on a null array whose element descriptor is
/// `value_desc`, as HotSpot 25 raises it from the `VarHandle<T>s$Array` body:
/// plain `get` / `set` index the array (`Cannot load from int array` /
/// `Cannot store to object array`), every other mode checks the index
/// against `array.length` first (`Cannot read the array length`). The local
/// is named `array` in each body.
pub fn vh_null_array_message(mode: &str, value_desc: &str) -> String {
    let elem = match value_desc.as_bytes().first() {
        Some(b'I') => "int",
        Some(b'J') => "long",
        Some(b'S') => "short",
        Some(b'C') => "char",
        Some(b'F') => "float",
        Some(b'D') => "double",
        Some(b'B' | b'Z') => "byte/boolean",
        _ => "object",
    };
    match mode {
        "get" => format!("Cannot load from {elem} array because \"array\" is null"),
        "set" => format!("Cannot store to {elem} array because \"array\" is null"),
        _ => "Cannot read the array length because \"array\" is null".to_string(),
    }
}

/// Does the view `meta` (a byte-array or `ByteBuffer` view) refuse access mode
/// `mode` with `UnsupportedOperationException`? The mode half of
/// [`vh_view_mode_refusal`], split out (round 15 wave 3, lane vh4) so the
/// index-coordinate conversion ([`vh_has_index_coordinate`]) can leave that
/// refusal first, as HotSpot orders it.
pub fn vh_view_mode_unsupported(meta: &VarHandleMeta, mode: &str) -> bool {
    if mode == "get" || mode == "set" {
        return false;
    }
    if !matches!(
        meta.kind,
        VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE
    ) {
        return true;
    }
    let elem = *meta.field_desc.as_bytes().first().unwrap_or(&b'J');
    let ordered = matches!(
        mode,
        "getVolatile" | "getOpaque" | "getAcquire" | "setVolatile" | "setOpaque" | "setRelease"
    );
    let numeric_update = mode.starts_with("getAndAdd") || mode.starts_with("getAndBitwise");
    let supported = match elem {
        b'I' | b'J' => true,
        b'F' | b'D' => !numeric_update,
        _ => ordered,
    };
    !supported
}

/// The access modes a byte-array or `ByteBuffer` view admits, refused the way
/// JDK 25's `VarHandleByteArrayAs*` / `VarHandleByteBufferAs*` refuse them.
///
/// * A `byte[]` view (`MethodHandles.byteArrayViewVarHandle`) has `get` and
///   `set` only; every other mode is `UnsupportedOperationException: <mode>`.
///   Before this, the ordered modes answered and the read-modify-write modes
///   ran as element operations on ONE byte (`getAndAdd` on an `int` view wrote
///   a byte and returned a `Byte`).
/// * A `ByteBuffer` view (`byteBufferViewVarHandle`) supports every mode for
///   `int`/`long`, every mode but `getAndAdd*`/`getAndBitwise*` for
///   `float`/`double`, and only the ordered reads and writes for
///   `short`/`char`. Past `get`/`set`, HotSpot then checks, in this order: a
///   heap buffer (`IllegalStateException: Atomic access not supported for heap
///   buffer`), a read-only buffer on a write (`ReadOnlyBufferException`), the
///   index, and the alignment (`IllegalStateException: Misaligned access at
///   index: N`).
///
/// Interpreter round i1 wave 31
/// (`interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`, items 1
/// and 2). Field and array handles never reach this.
pub fn vh_view_mode_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    meta: &VarHandleMeta,
    mode: &'static str,
) -> Result<(), MethodCallFailed> {
    if mode == "get" || mode == "set" {
        return Ok(());
    }
    if vh_view_mode_unsupported(meta, mode) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: mode.to_string(),
        }
        .into());
    }
    let elem = *meta.field_desc.as_bytes().first().unwrap_or(&b'J');
    // A null buffer is the leading-coordinate check's to report.
    let Some(Value::Object(Some(bb))) = args.get(1).copied() else {
        return Ok(());
    };
    if matches!(ctx.get_field_by_name(bb, "hb"), Value::Object(Some(_))) {
        return Err(RuntimeError::IllegalStateException {
            message: "Atomic access not supported for heap buffer".to_string(),
        }
        .into());
    }
    let writes = !mode.starts_with("get") || mode.starts_with("getAnd");
    if writes && byte_buffer_view_is_read_only(ctx, bb) {
        return Err(RuntimeError::ReadOnlyBufferException.into());
    }
    let Some(Value::Int(index)) = args.get(2).copied() else {
        return Ok(());
    };
    let idx = match byte_buffer_view_check_index(ctx, bb, index, elem) {
        Ok(i) => i,
        Err(fallback) => {
            // Raised through the JDK body of `mode` (fuzz14b), like `get`/`set`.
            let this = obj_arg(args, 0)?;
            let view = Some((elem, meta.kind == VH_KIND_BYTE_BUFFER_VIEW_LE, true));
            let at = Value::Object(Some(bb));
            return Err(vh_index_raise_through_jdk(
                ctx, this, view, mode, at, index, fallback,
            ));
        }
    };
    let width = byte_view_width(elem) as i64;
    if let Some(addr) = byte_buffer_view_addr(ctx, bb, idx) {
        if addr & (width - 1) != 0 {
            return Err(RuntimeError::IllegalStateException {
                message: format!("Misaligned access at index: {index}"),
            }
            .into());
        }
    }
    Ok(())
}

/// Whether [`vh_check_arity`]'s kind is a `ByteBuffer` view's.
#[inline]
pub fn is_byte_buffer_view_kind(kind: Option<i32>) -> bool {
    matches!(
        kind,
        Some(VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE)
    )
}

/// A read-modify-write access mode on a `ByteBuffer` view.
#[derive(Clone, Copy)]
pub enum VhViewRmw {
    CompareAndSet,
    CompareAndExchange,
    GetAndSet,
    GetAndAdd,
    Bitwise(VhBitOp),
}

/// A read-modify-write access through a `ByteBuffer` view, as one hardware
/// atomic on the direct buffer's memory. [`vh_view_mode_refusal`] has already
/// refused every mode, buffer and index HotSpot refuses, so the buffer here is
/// direct and the element aligned.
///
/// Before this, the natives' kind switches did not know the buffer kinds:
/// `compareAndSet` answered `false`, `compareAndExchange`/`getAndSet` `null`,
/// `getAndAdd`/`getAndBitwise*` `0`, and none of them wrote -- so a CAS retry
/// loop over a direct buffer never ended.
///
/// The word is compared and swapped in the buffer's own byte order: the
/// expected and new values are encoded with [`byte_view_encode`] (which
/// honours the view's endianness), so the atomic compares the bytes memory
/// holds. A `float`/`double` compares its raw bits, as HotSpot's does. A
/// buffer whose memory is not an Unsafe-arena block this VM can address
/// directly is `UnsupportedOperationException` rather than a silent no-op.
pub fn byte_buffer_view_rmw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: VhViewRmw,
) -> MethodCallResult {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    let this = obj_arg(args, 0)?;
    let meta = vh_meta_get(ctx, this);
    let Some((elem, le)) = byte_buffer_view_kind(meta.as_deref()) else {
        return Ok(Some(Value::Object(None)));
    };
    let mut bb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    let index = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let idx = byte_buffer_view_check_index(ctx, bb, index, elem)?;
    let _window = byte_buffer_view_access_window(ctx, &mut bb)?;
    let width = byte_view_width(elem);
    let refuse = || -> MethodCallFailed {
        RuntimeError::UnsupportedOperationException {
            message: "atomic access to this buffer's memory is not supported".to_string(),
        }
        .into()
    };
    let addr = byte_buffer_view_addr(ctx, bb, idx).ok_or_else(refuse)?;
    let ptr =
        crate::unsafe_natives_ext::unsafe_arena_access_ptr(addr, width).ok_or_else(refuse)?;
    if (ptr as usize) % width != 0 || !(width == 4 || width == 8) {
        return Err(refuse());
    }
    // `word(v)`: the bytes `v` occupies in the buffer, read as a native word.
    let word = |v: &Value| -> u64 {
        let bytes = byte_view_encode(elem, le, v);
        if width == 4 {
            u64::from(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        } else {
            u64::from_ne_bytes(bytes)
        }
    };
    let value = |w: u64| -> Value {
        if width == 4 {
            byte_view_decode(elem, le, &(w as u32).to_ne_bytes())
        } else {
            byte_view_decode(elem, le, &w.to_ne_bytes())
        }
    };
    // SAFETY: `unsafe_arena_real_ptr_bounded` answered for at least `width`
    // live bytes, and the pointer is `width`-aligned (checked above).
    let cas = |current: u64, new: u64| -> Result<u64, u64> {
        if width == 4 {
            let cell = unsafe { AtomicU32::from_ptr(ptr as *mut u32) };
            cell.compare_exchange(
                current as u32,
                new as u32,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map(u64::from)
            .map_err(u64::from)
        } else {
            let cell = unsafe { AtomicU64::from_ptr(ptr as *mut u64) };
            cell.compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
        }
    };
    let load = || -> u64 {
        if width == 4 {
            u64::from(unsafe { AtomicU32::from_ptr(ptr as *mut u32) }.load(Ordering::SeqCst))
        } else {
            unsafe { AtomicU64::from_ptr(ptr as *mut u64) }.load(Ordering::SeqCst)
        }
    };
    let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Int(0));
    match op {
        VhViewRmw::CompareAndSet => {
            let ok = cas(word(&arg(3)), word(&arg(4))).is_ok();
            Ok(Some(Value::Int(ok as i32)))
        }
        VhViewRmw::CompareAndExchange => {
            let witness = match cas(word(&arg(3)), word(&arg(4))) {
                Ok(w) | Err(w) => w,
            };
            Ok(Some(value(witness)))
        }
        VhViewRmw::GetAndSet | VhViewRmw::GetAndAdd | VhViewRmw::Bitwise(_) => {
            let operand = arg(3);
            let mut current = load();
            loop {
                let old = value(current);
                let new = match (op, &old, &operand) {
                    (VhViewRmw::GetAndSet, _, v) => *v,
                    (VhViewRmw::GetAndAdd, Value::Int(a), Value::Int(b)) => {
                        Value::Int(a.wrapping_add(*b))
                    }
                    (VhViewRmw::GetAndAdd, Value::Long(a), Value::Long(b)) => {
                        Value::Long(a.wrapping_add(*b))
                    }
                    (VhViewRmw::GetAndAdd, Value::Long(a), Value::Int(b)) => {
                        Value::Long(a.wrapping_add(i64::from(*b)))
                    }
                    (VhViewRmw::Bitwise(bit), Value::Int(a), Value::Int(b)) => {
                        Value::Int(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    (VhViewRmw::Bitwise(bit), Value::Long(a), Value::Long(b)) => {
                        Value::Long(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    _ => return Err(refuse()),
                };
                match cas(current, word(&new)) {
                    Ok(_) => return Ok(Some(old)),
                    Err(witness) => current = witness,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `IndirectVarHandle` (round 14 wave 7, lane ffm7; proposal FFM6-1)
// ---------------------------------------------------------------------------
//
// Every VarHandle combinator (`MethodHandles.filterValue`, `filterCoordinates`,
// `insertCoordinates`, `collectCoordinates`, `dropCoordinates`,
// `permuteCoordinates`, and the FFM layout paths built on them) runs JDK
// bytecode that returns a `java.lang.invoke.IndirectVarHandle`. None of the
// access natives below knew the class: no side-table row, slot 0 is the real
// `vform`, so each fell to its positional `VH_KIND` read and answered `null` /
// stored nothing -- silently -- and an `(array, int, ...)` call shape was
// hijacked by `vh_array_call` as a raw array access
// (`r14w6-ffm6-indirect-var-handle-accesses-dropped-FIXED-20260929.md`).
//
// Served the way HotSpot serves it (`IndirectVarHandle.getMethodHandleUncached`
// and the `VarHandleGuards` indirect arm): the mode handle is
// `handleFactory.apply(mode, <target's mode handle>)`, typed
// `(VarHandle, coordinates..., values...)R`, invoked with `directTarget`
// leading. A non-indirect target's mode handle is the mode's own invoker --
// the handle `varhandle_to_method_handle` builds before its `bindTo` -- and an
// indirect target recurses. The composed handle is cached where the JDK caches
// it, in the handle's own `VarHandle.methodHandleTable[mode.ordinal()]`.
// Anything that cannot be served THROWS `UnsupportedOperationException`; it
// never answers `null`.

pub const INDIRECT_VAR_HANDLE_CLASS: &str = "java/lang/invoke/IndirectVarHandle";
pub const VAR_HANDLE_CLASS: &str = "java/lang/invoke/VarHandle";

/// `VarHandle.AccessMode` in ordinal order, `(enum name, method name)`: the
/// index is the ordinal and the `methodHandleTable` slot. The same 31 modes in
/// the same order on every JDK 9+ image.
pub const VH_ACCESS_MODES: [(&str, &str); 31] = [
    ("GET", "get"),
    ("SET", "set"),
    ("GET_VOLATILE", "getVolatile"),
    ("SET_VOLATILE", "setVolatile"),
    ("GET_ACQUIRE", "getAcquire"),
    ("SET_RELEASE", "setRelease"),
    ("GET_OPAQUE", "getOpaque"),
    ("SET_OPAQUE", "setOpaque"),
    ("COMPARE_AND_SET", "compareAndSet"),
    ("COMPARE_AND_EXCHANGE", "compareAndExchange"),
    ("COMPARE_AND_EXCHANGE_ACQUIRE", "compareAndExchangeAcquire"),
    ("COMPARE_AND_EXCHANGE_RELEASE", "compareAndExchangeRelease"),
    ("WEAK_COMPARE_AND_SET_PLAIN", "weakCompareAndSetPlain"),
    ("WEAK_COMPARE_AND_SET", "weakCompareAndSet"),
    ("WEAK_COMPARE_AND_SET_ACQUIRE", "weakCompareAndSetAcquire"),
    ("WEAK_COMPARE_AND_SET_RELEASE", "weakCompareAndSetRelease"),
    ("GET_AND_SET", "getAndSet"),
    ("GET_AND_SET_ACQUIRE", "getAndSetAcquire"),
    ("GET_AND_SET_RELEASE", "getAndSetRelease"),
    ("GET_AND_ADD", "getAndAdd"),
    ("GET_AND_ADD_ACQUIRE", "getAndAddAcquire"),
    ("GET_AND_ADD_RELEASE", "getAndAddRelease"),
    ("GET_AND_BITWISE_OR", "getAndBitwiseOr"),
    ("GET_AND_BITWISE_OR_RELEASE", "getAndBitwiseOrRelease"),
    ("GET_AND_BITWISE_OR_ACQUIRE", "getAndBitwiseOrAcquire"),
    ("GET_AND_BITWISE_AND", "getAndBitwiseAnd"),
    ("GET_AND_BITWISE_AND_RELEASE", "getAndBitwiseAndRelease"),
    ("GET_AND_BITWISE_AND_ACQUIRE", "getAndBitwiseAndAcquire"),
    ("GET_AND_BITWISE_XOR", "getAndBitwiseXor"),
    ("GET_AND_BITWISE_XOR_RELEASE", "getAndBitwiseXorRelease"),
    ("GET_AND_BITWISE_XOR_ACQUIRE", "getAndBitwiseXorAcquire"),
];

/// The `AccessType.SET` modes (`set`, `setVolatile`, `setRelease`,
/// `setOpaque`): the access natives answer `void` for them.
pub fn vh_access_mode_is_set(ordinal: usize) -> bool {
    matches!(ordinal, 1 | 3 | 5 | 7)
}

/// Deepest combinator-over-combinator chain followed; a real one is a
/// handful deep, so this only bounds a malformed graph.
pub const VH_INDIRECT_MAX_DEPTH: u32 = 64;

/// `CRATONVM_VH_INDIRECT_SERVE=0`: an `IndirectVarHandle` access is REFUSED
/// (`UnsupportedOperationException`) instead of served through its
/// `handleFactory`, and `varType()`/`coordinateTypes()` refuse as before.
/// Default on, both modes (round 14 wave 7, lane ffm7).
pub fn vh_indirect_serve_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| true)
}

/// `CRATONVM_VH_INDIRECT_REFUSE=0`, together with
/// `CRATONVM_VH_INDIRECT_SERVE=0`: restore the pre-wave-7 answer (the access
/// falls through to the generic arms: `null` / nothing stored). Default on,
/// both modes; it has no effect while serving is on.
pub fn vh_indirect_refuse_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

pub fn is_indirect_var_handle(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        == Some(INDIRECT_VAR_HANDLE_CLASS)
}

pub fn vh_indirect_refusal(method: &str, why: &str) -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: format!("IndirectVarHandle.{method}: {why}"),
    }
    .into()
}

/// A reference field of an `IndirectVarHandle` (a real JDK class, so its own
/// layout, resolved by name).
pub fn indirect_vh_field(ctx: &mut dyn NativeContext, vh: ObjectRef, field: &str) -> Option<ObjectRef> {
    let idx = ctx.resolve_field_index(INDIRECT_VAR_HANDLE_CLASS, field)?;
    match ctx.get_field(vh, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// [`vh_indirect_serve`] with the parameter-site word the native took
/// (`poly_call_site::take_vh_param_site`, `0` = none armed), which decides the
/// `asType` refusals of the trailing value parameters (round 15 wave 1, lane
/// vh) and, since round 15 wave 3 (lane vh3; VH1-3), of the coordinates it
/// covers ([`vh_indirect_value_site_refusal`]); and over the row the prologue
/// read (interpreter wave 47, [`VARHANDLE_ONE_ROW_READ`]; `&None` = not read,
/// ask the side table).
pub fn vh_indirect_serve_site(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    site: u64,
    row: &VhPrologueRow,
) -> Option<MethodCallResult> {
    let vh = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let has_row = match row {
        Some(meta) => meta.is_some(),
        None => vh_meta_get(ctx, vh).is_some(),
    };
    if has_row || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    if !vh_indirect_serve_enabled() && !vh_indirect_refuse_enabled() {
        return None;
    }
    Some(vh_indirect_serve_on(ctx, vh, args, method, site))
}

pub fn vh_indirect_serve_on(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    method: &str,
    site: u64,
) -> MethodCallResult {
    let Some(ordinal) = VH_ACCESS_MODES.iter().position(|(_, m)| *m == method) else {
        return Err(vh_indirect_refusal(method, "unknown access mode"));
    };
    // Wave 57 (lane L4): `asType`'s return direction against the combinator's
    // own variable type (its `value`), before the access, as the direct
    // handles' natives judge theirs (`vh_return_site_check`).
    if matches!(vh_access_type_of_method(method), Some(0 | 3 | 4)) {
        let ret = cratonvm_native_api::poly_call_site::vh_param_site_return(site);
        if ret != 0 {
            vh_return_site_check_cold(ctx, args, ret, &Some(None))?;
        }
    }
    if !vh_indirect_serve_enabled() {
        return Err(vh_indirect_refusal(
            method,
            "an access through a VarHandle combinator is not served \
             (CRATONVM_VH_INDIRECT_SERVE=0)",
        ));
    }
    // SBR5 lane avh: a repeat access of a JDK `arrayElementVarHandle` (nothing allocated or
    // run in Java: the verdict and screens of its first access are filed).
    if let Some(answer) = vh_array_element_early(ctx, vh, args, ordinal, site) {
        return answer;
    }
    // Everything below allocates or runs Java: pin the handle and every
    // reference argument, and re-read each before use.
    let base = ctx.pin_native_root(vh);
    let mut arg_pins: Vec<Option<usize>> = Vec::with_capacity(args.len());
    for v in &args[1..] {
        arg_pins.push(match v {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        });
    }
    let result = vh_indirect_dispatch(ctx, base, vh, args, &arg_pins, ordinal, site);
    ctx.unpin_native_roots(base);
    result
}

/// A primitive argument boxed for `invokeWithArguments`: as the declared
/// parameter's primitive when the call site's value converts to it (an `int`
/// slot also carries `boolean`/`byte`/`short`/`char`; `asType` widens `int`
/// to `long`/`float`/`double`), else as its own kind. `None` for a reference.
pub fn vh_indirect_box_input(value: Value, declared: Option<&str>) -> Option<(Value, &'static str)> {
    Some(match (value, declared.unwrap_or("")) {
        (Value::Int(_), "Z") => (value, "Z"),
        (Value::Int(_), "B") => (value, "B"),
        (Value::Int(_), "S") => (value, "S"),
        (Value::Int(_), "C") => (value, "C"),
        (Value::Int(i), "J") => (Value::Long(i64::from(i)), "J"),
        (Value::Int(i), "F") => (Value::Float(i as f32), "F"),
        (Value::Int(i), "D") => (Value::Double(f64::from(i)), "D"),
        (Value::Long(l), "F") => (Value::Float(l as f32), "F"),
        (Value::Long(l), "D") => (Value::Double(l as f64), "D"),
        (Value::Float(f), "D") => (Value::Double(f64::from(f)), "D"),
        (Value::Int(_), _) => (value, "I"),
        (Value::Long(_), _) => (value, "J"),
        (Value::Float(_), _) => (value, "F"),
        (Value::Double(_), _) => (value, "D"),
        _ => return None,
    })
}

/// `CRATONVM_VH_INDIRECT_PARAMS_MEMO` (default on, both modes; round 15 wave
/// 2, lane vh2; VH1-1 in `jit-r15-vh-proposals-RETIRED-20261003.md`): a composed mode handle's
/// declared parameter list is rendered once per `MethodType` and thread
/// ([`vh_indirect_declared_params`]) instead of on every access. `0` renders
/// per access again.
pub fn vh_indirect_params_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

thread_local! {
    /// VH1-1: [`vh_indirect_declared_params`]'s answers, keyed like
    /// [`vh_memo_lookup`]'s lines (VM, identity hash, address,
    /// [`VH_META_GENERATION`]) but by the mode handle's `MethodType`, whose
    /// `ptypes` are final: the parameter list is a pure function of it.
    pub static VH_INDIRECT_PARAMS_MEMO: std::cell::RefCell<Vec<VhMemoLine<Arc<[String]>>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// The declared parameter list of the mode handle `mh`, read from its `type`
/// field only (what [`mh_declared_descriptor`] renders), or `None` when it has
/// none or it does not render (an unknown signature, which the callers
/// accept). Memoised per thread (VH1-1): the render is a mirror-to-name
/// resolution and a `String` per parameter, paid on every `IndirectVarHandle`
/// access before this.
pub fn vh_indirect_declared_params(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<Arc<[String]>> {
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return None;
    };
    if !vh_indirect_params_memo_enabled() {
        return vh_method_type_params(ctx, mt);
    }
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(mt);
    let addr = mt.as_ptr() as usize;
    // Read BEFORE the render, as `vh_memo_lookup` does: a render that moves
    // `mt` files its line under a generation no later lookup matches.
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let hit = VH_INDIRECT_PARAMS_MEMO.with(|memo| {
        let memo = memo.borrow();
        memo.get(slot).and_then(|line| {
            let same = line.vm == vm
                && line.key == hash
                && line.addr == addr
                && line.generation == generation;
            if same {
                line.value.clone()
            } else {
                None
            }
        })
    });
    if hit.is_some() {
        return hit;
    }
    let params = vh_method_type_params(ctx, mt)?;
    VH_INDIRECT_PARAMS_MEMO.with(|memo| {
        if let Some(line) = memo.borrow_mut().get_mut(slot) {
            *line = VhMemoLine {
                vm,
                key: hash,
                addr,
                generation,
                value: Some(Arc::clone(&params)),
            };
        }
    });
    Some(params)
}

/// The parameter descriptors of the `MethodType` `mt`, uncached.
pub fn vh_method_type_params(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<Arc<[String]>> {
    let desc = methodtype_to_descriptor(ctx, mt)?;
    Some(Arc::from(
        crate::lang_class::parse_descriptor_param_and_return(&desc).0,
    ))
}

pub fn vh_indirect_dispatch(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    args: &[Value],
    arg_pins: &[Option<usize>],
    ordinal: usize,
    site: u64,
) -> MethodCallResult {
    let method = VH_ACCESS_MODES[ordinal].1;
    let mh = vh_indirect_mode_handle(ctx, vh, ordinal, 0)?;
    let mh_pin = ctx.pin_native_root(mh);
    let vh = ctx.read_native_pin(vh_pin, vh);
    let Some(direct) = indirect_vh_field(ctx, vh, "directTarget") else {
        return Err(vh_indirect_refusal(
            method,
            "the handle has no directTarget",
        ));
    };
    let direct_pin = ctx.pin_native_root(direct);
    let mh = ctx.read_native_pin(mh_pin, mh);
    let params: Option<Arc<[String]>> = vh_indirect_declared_params(ctx, mh);
    // The mode handle's parameters are `(VarHandle, coordinates..., values...)`
    // and `args` is `(handle, coordinates..., values...)`: one for one.
    if let Some(params) = &params {
        if params.len() != args.len() {
            let msg = format!(
                "cannot convert IndirectVarHandle.{method}: the call site passes {} argument(s) \
                 after the VarHandle, the access mode takes {}",
                args.len() - 1,
                params.len().saturating_sub(1)
            );
            // The door rewrites the text with both method types (wave 53, L4).
            let vh = ctx.read_native_pin(vh_pin, vh);
            return Err(vh_astype_refused(ctx, vh, |ctx| {
                crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/invoke/WrongMethodTypeException",
                    &msg,
                )
            }));
        }
        // SBR5 lane avh: a REAL composed mode handle is always screened for the typed
        // lane (below); the carrier's screen stays opt-in.
        let real_mode_handle = mh_is_real_handle(&*ctx, ctx.read_native_pin(mh_pin, mh));
        if site != 0 || vh_indirect_typed_enabled() || real_mode_handle {
            // `args`' references as they are NOW (composing the mode handle
            // ran Java); reading a pin does not allocate. Judged, never stored.
            let mut live: Vec<Value> = Vec::with_capacity(args.len());
            live.push(Value::Object(Some(ctx.read_native_pin(vh_pin, vh))));
            for (i, v) in args.iter().enumerate().skip(1) {
                live.push(match (v, arg_pins.get(i - 1).copied().flatten()) {
                    (Value::Object(Some(o)), Some(pin)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, *o)))
                    }
                    (other, _) => *other,
                });
            }
            // The value parameters' static call-site types (round 15 wave 1,
            // lane vh): `asType` refuses `int -> char`, which an `int` slot
            // cannot show on its own.
            if let Some(refusal) = vh_indirect_value_site_refusal(ctx, &live, params, ordinal, site)
            {
                return Err(refusal);
            }
            // F7-1: the mode handle invoked at its own type, with no
            // `Object[]` and no argument boxes, when every argument already is
            // that type.
            if vh_indirect_typed_enabled() || real_mode_handle {
                if let Some(typed) = vh_indirect_typed_args(ctx, &live, params) {
                    let mh = ctx.read_native_pin(mh_pin, mh);
                    if vh_indirect_typed_admits_handle(ctx, mh) {
                        let mut call: Vec<Value> = Vec::with_capacity(args.len());
                        call.push(Value::Object(Some(ctx.read_native_pin(direct_pin, direct))));
                        for (i, v) in typed.into_iter().enumerate() {
                            call.push(match (v, arg_pins.get(i).copied().flatten()) {
                                (Value::Object(Some(o)), Some(pin)) => {
                                    Value::Object(Some(ctx.read_native_pin(pin, o)))
                                }
                                (other, _) => other,
                            });
                        }
                        let mh = ctx.read_native_pin(mh_pin, mh);
                        // No call site: the arguments are the declared types,
                        // and the door boxes a primitive result against the
                        // declared return, the shape `invokeWithArguments`
                        // answers.
                        let result = mh_invoke_exact_door(ctx, mh, &call, None, false)?;
                        if vh_access_mode_is_set(ordinal) {
                            return Ok(None);
                        }
                        return Ok(Some(result.unwrap_or(Value::Object(None))));
                    }
                    // SBR5 lane avh: a REAL composed mode handle (the JDK's own
                    // `IndirectVarHandle` over `collectCoordinates` etc.) invoked at
                    // its own type: its spun entry runs with the arguments as they
                    // are, with no `Object[]`, no `asType` and no JDK
                    // `invokeWithArguments` (the 44 us per get+set pair of
                    // `arrayElementVarHandle`'s bytecode road).
                    if real_mode_handle {
                        let mut call: Vec<Value> = Vec::with_capacity(args.len());
                        call.push(Value::Object(Some(ctx.read_native_pin(direct_pin, direct))));
                        for (i, v) in typed.into_iter().enumerate() {
                            call.push(match (v, arg_pins.get(i).copied().flatten()) {
                                (Value::Object(Some(o)), Some(pin)) => {
                                    Value::Object(Some(ctx.read_native_pin(pin, o)))
                                }
                                (other, _) => other,
                            });
                        }
                        // The JDK's `MemoryLayout.arrayElementVarHandle` (a
                        // `collectCoordinates` of the layout's scale over its segment
                        // handle): the scale is `offset + index * byteSize`, so the
                        // access is the segment handle's own at that offset.
                        if let Some(answer) =
                            vh_array_element_forward(ctx, vh_pin, vh, ordinal, &call[1..], params)
                        {
                            return answer;
                        }
                        let mh = ctx.read_native_pin(mh_pin, mh);
                        if let Some(desc) = mh_real_declared_descriptor(ctx, mh) {
                            let lane =
                                mh_real_direct_fast(ctx, mh, &call, Some(&desc), false, false)
                                    .or_else(|| {
                                        mh_real_lambda_form_dispatch(
                                            ctx,
                                            mh,
                                            &call,
                                            Some(&desc),
                                            false,
                                        )
                                    });
                            if let Some(result) = lane {
                                let result = auto_box_return(ctx, result, &desc)?;
                                if vh_access_mode_is_set(ordinal) {
                                    return Ok(None);
                                }
                                return Ok(Some(result.unwrap_or(Value::Object(None))));
                            }
                        }
                    }
                }
            }
        }
    }
    // `invokeWithArguments` is `asType(genericMethodType(n))` and an exact
    // invocation: it unboxes and casts each argument to the handle's declared
    // type, so the call-site conversions happen there, not here.
    let mut elems: Vec<(Value, Option<usize>)> = Vec::with_capacity(args.len());
    for (i, v) in args.iter().enumerate().skip(1) {
        let pin = arg_pins.get(i - 1).copied().flatten();
        if let Value::Object(Some(o)) = v {
            elems.push((Value::Object(Some(*o)), pin));
            continue;
        }
        let declared_param = params.as_ref().and_then(|p| p.get(i)).map(String::as_str);
        match vh_indirect_box_input(*v, declared_param) {
            Some((value, desc)) => match box_value_canonical(ctx, value, desc) {
                Value::Object(Some(b)) => {
                    let pin = ctx.pin_native_root(b);
                    elems.push((Value::Object(Some(b)), Some(pin)));
                }
                other => elems.push((other, None)),
            },
            None => elems.push((*v, None)),
        }
    }
    let arr = ctx.new_array(ArrayElementType::Reference, args.len());
    let direct = ctx.read_native_pin(direct_pin, direct);
    ctx.set_array_element(arr, 0, Value::Object(Some(direct)));
    for (i, (v, pin)) in elems.iter().enumerate() {
        let v = match (v, pin) {
            (Value::Object(Some(o)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(*pin, *o)))
            }
            _ => *v,
        };
        ctx.set_array_element(arr, i + 1, v);
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    let result = ctx.invoke_virtual(
        mh,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(arr))],
    )?;
    if vh_access_mode_is_set(ordinal) {
        return Ok(None);
    }
    // A primitive result comes back boxed, the shape every access native here
    // answers with; the call site's descriptor unboxes it.
    Ok(Some(result.unwrap_or(Value::Object(None))))
}

