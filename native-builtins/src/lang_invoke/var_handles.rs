// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// =============================================================================
// Record deserialization (java.io.ObjectStreamClass$RecordSupport)
// =============================================================================

/// Native for `ObjectStreamClass$RecordSupport.deserializationCtr(ObjectStreamClass)`.
///
/// The real JDK builds a `MethodHandle` adapter out of `foldArguments` /
/// `insertArguments` / `arrayElementGetter` combinators (see
/// `ObjectStreamClass.RecordSupport.deserializationCtr`), then
/// `ObjectInputStream.readRecord` invokes it as
/// `(Object) ctrMH.invokeExact(byte[] primValues, Object[] objValues)`.
/// CratonVM's synthetic MethodHandles cannot execute that combinator algebra
/// (it bottoms out in `MethodHandle.copyWith`, abstract → AbstractMethodError),
/// so instead we return a synthetic `MH_KIND_RECORD_DESER` handle that carries
/// the record class (`MH_CLASS`) + the describing `ObjectStreamClass`
/// (`MH_BOUND`); the dispatch arm rebuilds the record reflectively at invoke
/// time. Without this, every record (de)serialization fails — e.g. Tomcat's
/// `GenericPrincipal.writeReplace()` emits a `SerializablePrincipal` record
/// (catalina `TestGenericPrincipal`).
pub(crate) fn native_record_support_deserialization_ctr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let desc = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-SAFETY: `desc` is used again below (stored into the new
    // MethodHandle's MH_BOUND slot) after `ctx.invoke_virtual` (Java
    // dispatch) and `alloc_method_handle`'s own internal allocations,
    // either of which can trigger a moving GC. Pin it up front and re-read
    // before the final use.
    let desc_pin = ctx.pin_native_root(desc);
    let cls_mirror = match ctx.invoke_virtual(desc, "forClass", "()Ljava/lang/Class;", &[])? {
        Some(Value::Object(Some(m))) => m,
        _ => {
            ctx.unpin_native_roots(desc_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let cls_name = crate::lang_class::mirror_class_name(ctx, cls_mirror).unwrap_or_default();
    if cls_name.is_empty() {
        ctx.unpin_native_roots(desc_pin);
        return Ok(Some(Value::Object(None)));
    }
    let mh: ObjectRef = alloc_method_handle(
        ctx,
        &cls_name,
        "<init>",
        "([B[Ljava/lang/Object;)Ljava/lang/Object;",
        MH_KIND_RECORD_DESER,
    )?;
    let desc = ctx.read_native_pin(desc_pin, desc);
    ctx.set_field(mh, MH_BOUND, Value::Object(Some(desc)));
    ctx.unpin_native_roots(desc_pin);
    Ok(Some(Value::Object(Some(mh))))
}

/// Body of the `MH_KIND_RECORD_DESER` dispatch arm: rebuild a record instance
/// from the deserialized field arrays. `mh` carries the record class
/// (`MH_CLASS`) and describing `ObjectStreamClass` (`MH_BOUND`); `extra_args`
/// are `[primValues:byte[], objValues:Object[]]` from `readRecord`.
///
/// Maps each canonical record component (declaration order, from
/// `Class.getRecordComponents`) to its stream field BY NAME (the stream may
/// reorder fields — primitives first, then objects), pulling reference values
/// from `objValues` and decoding primitive values big-endian out of
/// `primValues`, then invokes the canonical constructor.
pub(crate) fn record_deser_dispatch(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
) -> MethodCallResult {
    let record_class = match mh_read_class(ctx, mh) {
        Some(c) if !c.is_empty() => c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let desc = match ctx.get_field(mh, MH_BOUND) {
        Value::Object(Some(d)) => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let prim_values = match extra_args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let obj_values = match extra_args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };

    // Index the stream fields by name. `objValues` holds the non-primitive
    // fields in stream order (k-th object field → objValues[k]); `primValues`
    // holds primitive bytes at each field's reported offset.
    let fields =
        match ctx.invoke_virtual(desc, "getFields", "()[Ljava/io/ObjectStreamField;", &[])? {
            Some(Value::Object(Some(a))) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
    let nfields = ctx.array_length(fields);
    // GC-safety: the per-field `invoke_virtual` calls below (getName /
    // isPrimitive / getTypeCode / getOffset) can each trigger a collection
    // that relocates `fields`/`prim_values`/`obj_values` (all captured once
    // and read again on EVERY later loop iteration below). Pin them for the
    // whole function and re-read the forwarded reference before each use.
    let fields_pin = ctx.pin_native_root(fields);
    let prim_values_pin = prim_values.map(|a| ctx.pin_native_root(a));
    let obj_values_pin = obj_values.map(|a| ctx.pin_native_root(a));
    // name -> (is_primitive, slot, type_code) where slot = objValues index
    // (reference) or primValues byte offset (primitive).
    let mut field_src: std::collections::HashMap<String, (bool, usize, char)> =
        std::collections::HashMap::with_capacity(nfields);
    let mut obj_index = 0usize;
    for i in 0..nfields {
        let fields = ctx.read_native_pin(fields_pin, fields);
        let f = match ctx.get_array_element(fields, i) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        // GC-safety: each `invoke_virtual` call below can trigger a
        // collection that relocates `f`, read again by the NEXT call in
        // this same chain. Pin per-iteration and release before the next
        // iteration (must not accumulate across iterations).
        let f_pin = ctx.pin_native_root(f);
        let fname = match ctx.invoke_virtual(f, "getName", "()Ljava/lang/String;", &[])? {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            _ => {
                ctx.unpin_native_roots(f_pin);
                continue;
            }
        };
        let f = ctx.read_native_pin(f_pin, f);
        let is_prim = matches!(
            ctx.invoke_virtual(f, "isPrimitive", "()Z", &[])?,
            Some(Value::Int(1))
        );
        let f = ctx.read_native_pin(f_pin, f);
        let tc = match ctx.invoke_virtual(f, "getTypeCode", "()C", &[])? {
            Some(Value::Int(c)) => char::from_u32(c as u32).unwrap_or('L'),
            _ => 'L',
        };
        if is_prim {
            let f = ctx.read_native_pin(f_pin, f);
            let offset = match ctx.invoke_virtual(f, "getOffset", "()I", &[])? {
                Some(Value::Int(n)) => n.max(0) as usize,
                _ => 0,
            };
            field_src.insert(fname, (true, offset, tc));
        } else {
            field_src.insert(fname, (false, obj_index, tc));
            obj_index += 1;
        }
        ctx.unpin_native_roots(f_pin);
    }

    // Enumerate canonical components (declaration order) → build ctor args + desc.
    let cls_mirror = match ctx.class_id_by_name(&record_class) {
        Some(cid) => ctx.get_class_mirror(cid),
        None => return Ok(Some(Value::Object(None))),
    };
    let comps = match ctx.invoke_virtual(
        cls_mirror,
        "getRecordComponents",
        "()[Ljava/lang/reflect/RecordComponent;",
        &[],
    )? {
        Some(Value::Object(Some(a))) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ncomp = ctx.array_length(comps);
    // GC-safety: same cross-iteration risk as the `fields` loop above, now
    // for `comps` (the still-pinned `obj_values`/`prim_values` are re-read
    // through their own pins inside the arm that uses each).
    let comps_pin = ctx.pin_native_root(comps);
    let mut ctor_desc = String::from("(");
    let mut ctor_args: Vec<Value> = Vec::with_capacity(ncomp);
    for j in 0..ncomp {
        let comps = ctx.read_native_pin(comps_pin, comps);
        let comp = match ctx.get_array_element(comps, j) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // GC-safety: `getType` below can trigger a collection that
        // relocates `comp` (read again by that same call); pin
        // per-iteration and release before the next iteration.
        let comp_pin = ctx.pin_native_root(comp);
        let cname = match ctx.invoke_virtual(comp, "getName", "()Ljava/lang/String;", &[])? {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Object(None))),
        };
        let comp = ctx.read_native_pin(comp_pin, comp);
        let ctype_mirror = match ctx.invoke_virtual(comp, "getType", "()Ljava/lang/Class;", &[])? {
            Some(Value::Object(Some(m))) => m,
            _ => return Ok(Some(Value::Object(None))),
        };
        ctx.unpin_native_roots(comp_pin);
        let comp_desc = mirror_to_descriptor(ctx, ctype_mirror).into_owned();
        ctor_desc.push_str(&comp_desc);

        let value = match field_src.get(&cname) {
            Some(&(false, idx, _)) => match obj_values {
                Some(arr) => {
                    let arr = obj_values_pin
                        .map(|p| ctx.read_native_pin(p, arr))
                        .unwrap_or(arr);
                    if idx < ctx.array_length(arr) {
                        ctx.get_array_element(arr, idx)
                    } else {
                        Value::Object(None)
                    }
                }
                _ => Value::Object(None),
            },
            Some(&(true, offset, tc)) => match prim_values {
                Some(arr) => {
                    let arr = prim_values_pin
                        .map(|p| ctx.read_native_pin(p, arr))
                        .unwrap_or(arr);
                    record_decode_primitive(ctx, arr, offset, tc)
                }
                None => default_for_descriptor(&comp_desc),
            },
            None => default_for_descriptor(&comp_desc),
        };
        // Interpreter round i4 wave 98, lane L8: the JDK's extractor for a
        // reference component ends in `asType(changeReturnType(pType))`, a
        // cast of the stream value to the component type, so a stream whose
        // field of that name holds another type fails with `ClassCastException`
        // (`Cannot cast X to Y`), which `readRecord` turns into
        // `InvalidObjectException(e.getMessage(), e)`. This passed the value
        // through unchecked and built a record whose component held an
        // instance of the wrong class. Components are cast in declaration
        // order (the folds run component 0 first), so the first one reported
        // is the first that fails. Probe
        // `tools/probes/interp/L8/L8W98RecordStreamShapes.java`.
        if !comp_desc.starts_with('L') && !comp_desc.starts_with('[') {
            // A primitive component: the stream field matched a primitive
            // (`matchFields` refuses a reference/primitive mix earlier).
        } else if reference_arg_admitted(ctx, value, &comp_desc) == Some(false) {
            let failure = reference_cast_failure(ctx, value, &comp_desc);
            ctx.unpin_native_roots(fields_pin);
            return Err(failure);
        }
        ctor_args.push(value);
    }
    ctor_desc.push_str(")V");
    ctx.unpin_native_roots(fields_pin);

    // Allocate + run the canonical constructor.
    let cid = ctx.ensure_class_initialized(&record_class)?;
    let new_obj = ctx.alloc_object(cid, ncomp.max(16));
    // GC-safety: the `<init>` invocation below can itself allocate; pin
    // `new_obj` and re-read the forwarded reference before it's returned.
    let new_obj_pin = ctx.pin_native_root(new_obj);
    let mut init_args = Vec::with_capacity(1 + ctor_args.len());
    init_args.push(Value::Object(Some(new_obj)));
    init_args.extend_from_slice(&ctor_args);
    ctx.invoke(&record_class, "<init>", &ctor_desc, &init_args)?;
    let new_obj = ctx.read_native_pin(new_obj_pin, new_obj);
    ctx.unpin_native_roots(new_obj_pin);
    Ok(Some(Value::Object(Some(new_obj))))
}

/// Decode a single primitive value, big-endian, out of the record stream's
/// `primValues` byte[] at `offset` per JVM type code (matches
/// `jdk.internal.util.ByteArray` big-endian layout used by record serialization).
pub(crate) fn record_decode_primitive(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    offset: usize,
    tc: char,
) -> Value {
    let len = ctx.array_length(arr);
    let b = |i: usize| -> u64 {
        if offset + i < len {
            match ctx.get_array_element(arr, offset + i) {
                Value::Int(v) => (v as u8) as u64,
                _ => 0,
            }
        } else {
            0
        }
    };
    match tc {
        'Z' => Value::Int(if b(0) != 0 { 1 } else { 0 }),
        'B' => Value::Int(b(0) as u8 as i8 as i32),
        'C' => Value::Int((((b(0) << 8) | b(1)) as u16) as i32),
        'S' => Value::Int(((((b(0) << 8) | b(1)) as u16) as i16) as i32),
        'I' => Value::Int((((b(0) << 24) | (b(1) << 16) | (b(2) << 8) | b(3)) as u32) as i32),
        'F' => Value::Float(f32::from_bits(
            ((b(0) << 24) | (b(1) << 16) | (b(2) << 8) | b(3)) as u32,
        )),
        'J' => Value::Long(
            (((b(0) << 56)
                | (b(1) << 48)
                | (b(2) << 40)
                | (b(3) << 32)
                | (b(4) << 24)
                | (b(5) << 16)
                | (b(6) << 8)
                | b(7)) as u64) as i64,
        ),
        'D' => Value::Double(f64::from_bits(
            (b(0) << 56)
                | (b(1) << 48)
                | (b(2) << 40)
                | (b(3) << 32)
                | (b(4) << 24)
                | (b(5) << 16)
                | (b(6) << 8)
                | b(7),
        )),
        _ => Value::Int(0),
    }
}

/// Default (zero/null) value for a field descriptor, used when a record
/// component has no matching stream field.
pub(crate) fn default_for_descriptor(desc: &str) -> Value {
    match desc.chars().next() {
        Some('J') => Value::Long(0),
        Some('F') => Value::Float(0.0),
        Some('D') => Value::Double(0.0),
        Some('L') | Some('[') => Value::Object(None),
        _ => Value::Int(0),
    }
}

/// C26: Insert `extra_n` erased parameter descriptors at position `pos`
/// in `inner_desc`. Each inserted parameter's descriptor is derived from
/// its Class mirror; if unavailable, falls back to `Ljava/lang/Object;`.
pub(crate) fn widen_descriptor(
    ctx: &dyn NativeContext,
    inner_desc: &str,
    extra_classes: cratonvm_types::ObjectRef,
    pos: usize,
) -> String {
    if !inner_desc.starts_with('(') {
        return inner_desc.to_string();
    }
    let close = match desc_close_paren(inner_desc) {
        Some(i) => i,
        None => return inner_desc.to_string(),
    };
    let params_str = &inner_desc[1..close];
    let ret_str = &inner_desc[close..]; // includes ')'
    let orig_types = parse_descriptor_types(params_str);
    let mut all: Vec<Cow<'static, str>> = orig_types
        .iter()
        .map(|n| class_name_to_descriptor(n))
        .collect();
    let extra_n = ctx.array_length(extra_classes);
    let pos_c = pos.min(all.len());
    let mut inserts: Vec<Cow<'static, str>> = Vec::with_capacity(extra_n);
    for i in 0..extra_n {
        let d = match ctx.get_array_element(extra_classes, i) {
            Value::Object(Some(mirror)) => mirror_to_descriptor(ctx, mirror),
            _ => Cow::Borrowed(DESC_OBJECT),
        };
        inserts.push(d);
    }
    all.splice(pos_c..pos_c, inserts);
    let mut out = String::with_capacity(inner_desc.len() + 8);
    out.push('(');
    for p in &all {
        out.push_str(p);
    }
    out.push_str(ret_str);
    out
}

/// Count the number of parameters in a JVM method descriptor.
pub(crate) fn count_descriptor_params(desc: &str) -> usize {
    if !desc.starts_with('(') {
        return 0;
    }
    let close = match desc_close_paren(desc) {
        Some(i) => i,
        None => return 0,
    };
    let params_str = &desc[1..close];
    parse_descriptor_types(params_str).len()
}

/// Type adaptation for invoke(): coerce args to match the expected descriptor.
/// Handles boxing (int→Integer), unboxing (Integer→int), and widening (int→long).
pub(crate) fn pin_mh_args(ctx: &mut dyn NativeContext, args: &[Value]) -> (usize, Vec<usize>) {
    let mut base = usize::MAX;
    let handles = args
        .iter()
        .map(|arg| match arg {
            Value::Object(Some(obj)) => {
                let h = ctx.pin_native_root(*obj);
                if base == usize::MAX {
                    base = h;
                }
                h
            }
            _ => usize::MAX,
        })
        .collect();
    (base, handles)
}

#[inline]
pub(crate) fn read_pinned_mh_arg(ctx: &dyn NativeContext, handle: usize, arg: Value) -> Value {
    match arg {
        Value::Object(Some(obj)) if handle != usize::MAX => {
            Value::Object(Some(ctx.read_native_pin(handle, obj)))
        }
        _ => arg,
    }
}

/// gen r4w3/rooting: re-read a whole arg slice pinned by [`pin_mh_args`]
/// (`handles` is that call's second result), yielding current addresses.
pub(crate) fn read_pinned_mh_args(ctx: &dyn NativeContext, handles: &[usize], args: &[Value]) -> Vec<Value> {
    args.iter()
        .zip(handles.iter())
        .map(|(arg, h)| read_pinned_mh_arg(ctx, *h, *arg))
        .collect()
}

/// gen r4w3/rooting: pin one (possibly non-object) value; `usize::MAX` for a
/// non-object, matching the handle convention of [`pin_mh_args`].
pub(crate) fn pin_mh_value(ctx: &mut dyn NativeContext, v: Value) -> usize {
    match v {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    }
}

pub(crate) fn adapt_invoke_args(ctx: &mut dyn NativeContext, args: &[Value], desc: &str) -> Vec<Value> {
    if desc.is_empty() || !desc.starts_with('(') {
        return args.to_vec();
    }
    let close = match desc_close_paren(desc) {
        Some(i) => i,
        None => return args.to_vec(),
    };
    let params_str = &desc[1..close];
    let param_types = parse_descriptor_types(params_str);

    let (pin_base, handles) = pin_mh_args(ctx, args);
    let mut result = Vec::with_capacity(args.len());
    for (i, arg) in args.iter().enumerate() {
        let arg = read_pinned_mh_arg(ctx, handles[i], *arg);
        if i < param_types.len() {
            result.push(adapt_single_arg(ctx, arg, &param_types[i]));
        } else {
            result.push(arg);
        }
    }
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    result
}

/// Adapt a single argument value to match the expected type.
///
/// Unboxes a boxed primitive wrapper (e.g. `Integer` → `int`) when the param
/// is a primitive — callers that pass arguments through a generic `Object[]`
/// (Jackson 3 invoking a record/POJO canonical constructor via
/// `MethodHandle.invokeExact`) hand us boxed values; without unboxing a boxed
/// `Integer` reached the `<init>` frame as an object reference and the int
/// field was stored as the wrapper's pointer. `unbox_value` returns non-wrapper
/// objects unchanged, so a misaligned receiver/object arg is left intact.
pub(crate) fn adapt_single_arg(ctx: &mut dyn NativeContext, arg: Value, expected_type: &str) -> Value {
    match expected_type {
        // Unbox-only primitives (no widening from another primitive tag).
        "int" | "short" | "byte" | "char" | "boolean" => match arg {
            Value::Object(Some(obj)) => crate::lang_class::unbox_value(ctx, obj),
            other => other,
        },
        // Widening: int → long (or unbox a Long/Integer wrapper). An unboxed
        // narrower wrapper is widened too (wave 25): `Integer` -> `long`
        // reached the callee as a `Value::Int` in a `long` slot, where HotSpot's
        // `asType` converts it (`tl.invoke((Object) Integer.valueOf(7))` = 7).
        "long" => match arg {
            Value::Int(v) => Value::Long(v as i64),
            Value::Object(Some(obj)) => {
                widen_primitive_to_descriptor(crate::lang_class::unbox_value(ctx, obj), DESC_LONG)
            }
            other => other,
        },
        // Widening: int → float (or unbox a Float/Integer wrapper).
        "float" => match arg {
            Value::Int(v) => Value::Float(v as f32),
            Value::Long(v) => Value::Float(v as f32),
            Value::Object(Some(obj)) => {
                widen_primitive_to_descriptor(crate::lang_class::unbox_value(ctx, obj), DESC_FLOAT)
            }
            other => other,
        },
        // Widening: int → double, long → double, float → double (or unbox).
        "double" => match arg {
            Value::Int(v) => Value::Double(v as f64),
            Value::Long(v) => Value::Double(v as f64),
            Value::Float(v) => Value::Double(v as f64),
            Value::Object(Some(obj)) => {
                widen_primitive_to_descriptor(crate::lang_class::unbox_value(ctx, obj), DESC_DOUBLE)
            }
            other => other,
        },
        // Narrowing not done automatically (invoke is lenient but not that lenient)
        _ => arg,
    }
}

/// The call site's static parameter types for an argument list a leaf arm of
/// [`mh_dispatch`] (or the `invoke` door) is about to convert, aligned with
/// that list: `lead` leading slots the site did not name (a bound value) read
/// as unknown (`""`). `None` when there is no published site -- only the
/// OUTERMOST handle sees one, [`take_entry_call_site`] -- or when the counts
/// do not line up (a varargs collection reshaped the list). The types only
/// word a `null`'s NPE and keep a site-typed primitive on the plain unbox; see
/// [`adapt_leaf_arg`].
pub(crate) fn leaf_site_params(site: Option<&str>, lead: usize, total: usize) -> Option<Vec<String>> {
    let (params, _) = split_descriptor_params(site?)?;
    if params.len() + lead != total {
        return None;
    }
    let mut out = vec![String::new(); lead];
    out.extend(params);
    Some(out)
}

/// One argument on its way to the parameter `pdesc` of a LEAF handle
/// (`MH_KIND_STATIC` / `VIRTUAL` / `SPECIAL` / `CONSTRUCTOR` / `SETTER`), or
/// to the declared parameter of an adapter entered through `invoke`:
/// `asType`'s conversion, fallible (interpreter round i1 wave 26, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// [`adapt_single_arg`] unboxed a wrapper and passed ANY other reference, and a
/// `null`, through to a primitive parameter: `insertArguments(sum, 0, 1)
/// .invoke((Object) "s")` ran `sum(int, int)` on the String's address. A
/// reference at a primitive parameter now takes [`mh_unbox_for_primitive`]
/// (HotSpot's `ValueConversions.unbox<W>(Object, false)`), except that a
/// wrapper whose primitive widens is converted here without building a
/// descriptor (the reflection path -- `DirectMethodHandleAccessor` hands every
/// primitive boxed -- meets that case on every call). A raw primitive is only
/// widened, and a reference parameter is left alone, both exactly as
/// [`adapt_single_arg`] did.
///
/// A site that typed the argument as a primitive passed it raw, so a reference
/// there is the VM's own, not the caller's, and keeps the plain unbox (the rule
/// [`invoke_reference_cast_refusal`] applies to the same case).
///
/// `explicit` is [`mh_explicit_scope`]: the dispatch runs under an
/// `explicitCastArguments` stamp, whose conversion is a JLS 5.5 cast -- a
/// `null` is the primitive's zero, a wrapper is unboxed and then widened OR
/// narrowed ([`explicit_cast_primitive`]; `Double` 2.9 -> `int` 2, `Boolean`
/// -> `int` 1, `Integer` 6 -> `boolean` false). Only a non-wrapper is refused
/// there, with `asType`'s own messages (measured on HotSpot 25.0.3).
pub(crate) fn adapt_leaf_arg(
    ctx: &mut dyn NativeContext,
    arg: Value,
    pdesc: &str,
    site_param: Option<&str>,
    explicit: bool,
) -> Result<Value, MethodCallFailed> {
    if wrapper_desc_for_primitive(pdesc).is_none() {
        return Ok(arg);
    }
    if explicit {
        return match arg {
            Value::Object(None) => Ok(mh_zero_value(pdesc)),
            Value::Object(Some(obj)) if !ctx.object_is_array(obj) => {
                match crate::lang_class::unbox_value(&*ctx, obj) {
                    // A non-wrapper is refused exactly as `asType` refuses it
                    // (measured: `String` -> `(Number)` cast, `BigInteger` ->
                    // `Cannot cast java.math.BigInteger to java.lang.Integer`).
                    Value::Object(_) => mh_unbox_for_primitive(ctx, pdesc, arg, site_param),
                    v => Ok(explicit_cast_primitive(v, pdesc)),
                }
            }
            Value::Object(Some(_)) => mh_unbox_for_primitive(ctx, pdesc, arg, site_param),
            raw => Ok(explicit_cast_primitive(raw, pdesc)),
        };
    }
    let obj = match arg {
        Value::Object(Some(obj)) => obj,
        Value::Object(None) => return mh_unbox_for_primitive(ctx, pdesc, arg, site_param),
        raw => return Ok(widen_primitive_to_descriptor(raw, pdesc)),
    };
    if site_param.is_some_and(|p| wrapper_desc_for_primitive(p).is_some()) {
        return Ok(widen_primitive_to_descriptor(
            crate::lang_class::unbox_value(&*ctx, obj),
            pdesc,
        ));
    }
    if !ctx.object_is_array(obj) {
        let src = match ctx.class_name_of_id(ctx.class_id_of_object(obj)).as_deref() {
            Some("java/lang/Integer") => Some("I"),
            Some("java/lang/Long") => Some("J"),
            Some("java/lang/Short") => Some("S"),
            Some("java/lang/Byte") => Some("B"),
            Some("java/lang/Character") => Some("C"),
            Some("java/lang/Boolean") => Some("Z"),
            Some("java/lang/Float") => Some("F"),
            Some("java/lang/Double") => Some("D"),
            _ => None,
        };
        if src.is_some_and(|src| primitive_widens_to(src, pdesc)) {
            return Ok(widen_primitive_to_descriptor(ctx.get_field(obj, 0), pdesc));
        }
    }
    // A non-widening wrapper, a non-wrapper, an array, or a class this
    // context cannot name: the full conversion decides (and words) it.
    mh_unbox_for_primitive(ctx, pdesc, arg, site_param)
}

/// The refusal a LEAF arm of [`mh_dispatch`] raises when it is handed FEWER
/// arguments than its member's descriptor has parameters (`have < need`), or
/// `None` (round 15 wave 5, lane mh4; memory safety, no switch).
///
/// The VM builds the callee frame from the arguments it is given and fills
/// every other local with the uninitialized marker, so a member entered one
/// argument short reads that marker as its missing parameter: an `aload` of it
/// is a bogus reference. `R15Mh3AdapterArity` row `filter-short`
/// (`filterArguments(cat2, 0, upper).invoke("a")`, a FILTER adapter called
/// with fewer arguments than its type) died with `EXCEPTION_ACCESS_VIOLATION`
/// reading address `0xFF4` in both modes and on every collector once the
/// adapter handed `cat2` one argument; the doors' arity refusals catch that
/// call first, but a crash must not depend on a correctness switch
/// (`CRATONVM_MH_INVOKE_ADAPTER_ARITY=0`) or on every adapter's `type` being
/// right. HotSpot refuses such a call at `asType` with
/// `WrongMethodTypeException`, so that is the class raised here; the message
/// names the member's type and the count, since the call site's type is not
/// known this deep. A SURPLUS is not refused: the VM widens the frame for it
/// (`effective_max_locals`), and adapters reach leaves that way today.
pub(crate) fn leaf_short_arity_refusal(
    ctx: &mut dyn NativeContext,
    desc: &str,
    need: usize,
    have: usize,
) -> Option<MethodCallFailed> {
    if have >= need {
        return None;
    }
    let shown = method_type_display_in(&*ctx, desc).unwrap_or_else(|| desc.to_string());
    let refusal = mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{shown} to a call with {have} argument(s)"),
    );
    // Wave 54: the door that knows the call site words it as HotSpot does.
    if let MethodCallFailed::ExceptionThrown(exc) = &refusal {
        MH_DEEP_ASTYPE_REFUSAL.with(|c| c.set(exc.as_ptr() as usize));
    }
    Some(refusal)
}

/// `invokeExact`'s `WrongMethodTypeException` text, JDK 25's
/// `Invokers.newWrongMethodTypeException` wording, over two types already
/// shown as `MethodType.toString()` shows them ([`method_type_display_in`]).
/// The one place the MethodHandle doors spell it (MH717-1, JIT round j27
/// wave 4, lane MHROWS2). Pure.
pub(crate) fn mh_exact_type_text(shown_declared: &str, shown_site: &str) -> String {
    format!("handle's method type {shown_declared} but found {shown_site}")
}

/// The door half of [`MH_DEEP_ASTYPE_REFUSAL`] (interpreter round i2 wave 54,
/// lane L4; `interpreter-L4-proposal-one-astype-refusal-record-for-every-invoke-door-FIXED-20261018`):
/// a door's error, with a LEAF arm's arity refusal reworded as the refusal
/// HotSpot raises at the door's own `asType` / exact-type check -- `cannot
/// convert MethodHandle<declared> to <site>` (`invoke`, `invokeWithArguments`),
/// `handle's method type <declared> but found <site>` (`invokeExact`) -- in
/// place of `... to a call with N argument(s)`, which named no call site. Only
/// the exception the leaf recorded is reworded; every error takes the record.
/// `CRATONVM_MH_ASTYPE_REFUSAL_SITE_TEXT`; `CRATONVM_DBG_MH_DISPATCH=1`
/// prints `[MH_ASTYPE_SITE] <text>` (the positive control).
#[cold]
#[inline(never)]
pub(crate) fn mh_deep_refusal_site_text(
    ctx: &mut dyn NativeContext,
    failed: MethodCallFailed,
    declared: Option<&str>,
    site: Option<&str>,
    exact: bool,
) -> MethodCallFailed {
    let noted = MH_DEEP_ASTYPE_REFUSAL.with(|c| c.replace(0));
    let exc = match &failed {
        MethodCallFailed::ExceptionThrown(exc) if noted != 0 && exc.as_ptr() as usize == noted => {
            *exc
        }
        _ => return failed,
    };
    if !crate::nbflags().mh_astype_refusal_site_text {
        return failed;
    }
    let (Some(declared), Some(site)) = (declared, site) else {
        return failed;
    };
    let (Some(shown_declared), Some(shown_site)) = (
        method_type_display_in(&*ctx, declared),
        method_type_display_in(&*ctx, site),
    ) else {
        return failed;
    };
    let text = if exact {
        mh_exact_type_text(&shown_declared, &shown_site)
    } else {
        format!("cannot convert MethodHandle{shown_declared} to {shown_site}")
    };
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_ASTYPE_SITE] {text}");
    }
    // The message string is an allocation: the exception is pinned across it.
    let pin = ctx.pin_native_root(exc);
    let message = ctx.create_string_uninterned(&text);
    let exc = ctx.read_native_pin(pin, exc);
    ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(message)));
    ctx.unpin_native_roots(pin);
    MethodCallFailed::ExceptionThrown(exc)
}

/// `asType`'s arity refusal for the kinds the `invoke` / `invokeWithArguments`
/// doors judged for `invokeExact` only (interpreter round i1 wave 40): a
/// `constant` / `zero` / `empty`, an `identity`, an array element getter /
/// setter / `arrayLength`, a `throwException` handle, each minted with its
/// exact `type` (`exact_call_site_refusal`), called at a site of another
/// arity. HotSpot 25 (`L4W54AsTypeRefusalSite`): `constant(int, 7).invoke(1)`
/// is `cannot convert MethodHandle()int to (int)int`; this door answered 7 (the
/// arm ignores its arguments), `identity(int).invoke()` read an absent
/// argument. A variable-arity collector (it collects at another arity) and the
/// fabricated `()V` stand-in `type` are not judged. Interpreter round i2 wave
/// 54, lane L4, behind `CRATONVM_MH_ASTYPE_REFUSAL_SITE_TEXT`.
pub(crate) fn invoke_exact_typed_kind_arity_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    site: &str,
    declared: Option<&str>,
) -> Option<MethodCallFailed> {
    if !matches!(
        kind,
        MH_KIND_CONSTANT
            | MH_KIND_IDENTITY
            | MH_KIND_ARRAY_GET
            | MH_KIND_ARRAY_SET
            | MH_KIND_ARRAY_LENGTH
            | MH_KIND_THROW
    ) {
        return None;
    }
    let declared = declared?;
    if declared == site
        || (declared == "()V" && site != "()V")
        || !crate::nbflags().mh_astype_refusal_site_text
        || mh_is_varargs_collector(&*ctx, mh)
    {
        return None;
    }
    let (declared_params, _) = split_descriptor_params(declared)?;
    let (site_params, _) = split_descriptor_params(site)?;
    if declared_params.len() == site_params.len() {
        return None;
    }
    let shown_declared = method_type_display_in(&*ctx, declared)?;
    let shown_site = method_type_display_in(&*ctx, site)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
    ))
}

/// [`adapt_invoke_args`] for a LEAF arm of [`mh_dispatch`], with
/// [`adapt_leaf_arg`]'s conversion: the first argument that does not convert
/// to its primitive parameter is refused as HotSpot refuses it, and (wave 27)
/// the first reference that does not cast to its reference parameter
/// ([`leaf_reference_refusal`], judged only on a 1:1 list). `args` align
/// 1:1 with `desc`'s parameters (the leaf arms strip a receiver and prepend a
/// bound value before calling this); `site` is [`leaf_site_params`]'s answer
/// for the same list.
///
/// Why the leaf and not only the `invoke` door: an ADAPTER (`insertArguments`,
/// `dropArguments`, `asSpreader`, `filterArguments`, ...) reaches its leaf
/// through `mh_dispatch`'s recursive arms, with arguments it bound, spread or
/// reordered itself, and an `asType`-retyped direct handle called with
/// `invokeExact` reaches it with the retyped (reference) arguments. Every
/// internal producer of a placeholder at a primitive position was given the
/// primitive's zero in the same change (`tryFinally`'s result slot, a loop
/// variable with no `init`), which is what HotSpot passes there.
pub(crate) fn adapt_leaf_args(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    desc: &str,
    site: Option<&[String]>,
    explicit: bool,
) -> Result<Vec<Value>, MethodCallFailed> {
    let Some((params, _)) = split_descriptor_params(desc) else {
        return Ok(args.to_vec());
    };
    if let Some(refusal) = leaf_short_arity_refusal(ctx, desc, params.len(), args.len()) {
        return Err(refusal);
    }
    // References are cast (wave 27, [`leaf_reference_refusal`]) only when the
    // list lines up 1:1 with the parameters, the rule
    // `invoke_reference_cast_refusal` applies: a list the varargs repair left
    // flat, or one an adapter reshaped, cannot be aligned, and guessing the
    // alignment is how a cast check starts refusing correct calls.
    let cast_references = args.len() == params.len();
    let (pin_base, handles) = pin_mh_args(ctx, args);
    let mut out = Vec::with_capacity(args.len());
    let mut refusal = None;
    // Round 15 wave 5 (lane mh4): the boxes `leaf_box_raw_for_reference`
    // minted, `(index, pin)`. A box allocates, so every reference already in
    // `out` is re-read once the loop is done.
    let mut box_pins: Vec<(usize, usize)> = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let arg = read_pinned_mh_arg(ctx, handles[i], *arg);
        let Some(pdesc) = params.get(i) else {
            out.push(arg);
            continue;
        };
        let site_param = site
            .and_then(|s| s.get(i))
            .map(String::as_str)
            .filter(|p| !p.is_empty());
        let arg = match leaf_box_raw_for_reference(ctx, arg, pdesc, site_param) {
            Some(boxed) => {
                if let Value::Object(Some(o)) = boxed {
                    box_pins.push((i, ctx.pin_native_root(o)));
                }
                boxed
            }
            None => arg,
        };
        match adapt_leaf_arg(ctx, arg, pdesc, site_param, explicit) {
            Ok(v) => {
                if cast_references {
                    if let Some(e) = leaf_reference_refusal(ctx, v, pdesc, explicit) {
                        refusal = Some(e);
                        break;
                    }
                }
                out.push(v);
            }
            Err(e) => {
                refusal = Some(e);
                break;
            }
        }
    }
    if !box_pins.is_empty() && refusal.is_none() {
        // A reference in `out` is either the caller's argument (pinned by
        // `pin_mh_args`; `adapt_leaf_arg` hands a reference parameter its
        // argument unchanged) or a box minted above.
        for (j, v) in out.iter_mut().enumerate() {
            *v = match box_pins.iter().find(|(k, _)| *k == j) {
                Some(&(_, pin)) => match *v {
                    Value::Object(Some(o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
                    other => other,
                },
                None => read_pinned_mh_arg(ctx, handles[j], *v),
            };
        }
    }
    let first_pin = if pin_base != usize::MAX {
        pin_base
    } else {
        box_pins.first().map_or(usize::MAX, |&(_, pin)| pin)
    };
    if first_pin != usize::MAX {
        ctx.unpin_native_roots(first_pin);
    }
    match refusal {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// `asType`'s BOXING conversion (primitive -> reference) for a RAW primitive
/// handed to a leaf's REFERENCE parameter `pdesc`, or `None` (round 15 wave 5,
/// lane mh4; `r15w5-upcall1-synthetic-upcall-dispatch-has-no-call-site-patch`;
/// memory safety, no switch).
///
/// [`adapt_leaf_arg`] converts only toward a primitive parameter, and the VM
/// lays a `Value::Int` down in the callee's local as an `int`, which the
/// member's `aload` then reads as a reference: an FFM upcall to
/// `findStatic(C, "addBoxed", (Integer,Integer)Integer).asType((int,int)int)`
/// (exactly the descriptor's type, so `upcallStub` accepts it) entered
/// `addBoxed` with an `int`'s bits in an `Integer` local. A Java call site of
/// a primitive type hands the doors raw primitives the same way. HotSpot's
/// adapter boxes with `valueOf`; so does this (`box_value_canonical`). The
/// box's class is the call site's primitive when the site names one
/// (`char` / `boolean` / `byte` / `short` share `Value::Int`), else the raw
/// value's own variant; a box the parameter does not admit is then the
/// ordinary reference cast's `ClassCastException`, never a raw word.
pub(crate) fn leaf_box_raw_for_reference(
    ctx: &mut dyn NativeContext,
    arg: Value,
    pdesc: &str,
    site_param: Option<&str>,
) -> Option<Value> {
    if !(pdesc.starts_with('L') || pdesc.starts_with('[')) {
        return None;
    }
    let variant = match arg {
        Value::Int(_) => "I",
        Value::Long(_) => "J",
        Value::Float(_) => "F",
        Value::Double(_) => "D",
        _ => return None,
    };
    let prim = match site_param {
        Some(p @ ("Z" | "B" | "C" | "S" | "I")) if variant == "I" => p,
        _ => variant,
    };
    Some(crate::lang_class::box_value_canonical(ctx, arg, prim))
}

/// `asType(callSiteType)` for the arguments of an ADAPTER handle entered
/// through `MethodHandle.invoke` (wave 26): each argument is converted to the
/// adapter's DECLARED parameter (its `type`, which is adapted for exactly the
/// kinds [`kind_has_authoritative_adapted_type`] adds), before any of the
/// adapter's own work runs -- a guard's test, a fold's combiner, a filter.
///
/// The door used to run [`adapt_invoke_args`] against the adapter's `MH_DESC`,
/// which is its LEAF's descriptor: misaligned for every adapter that changes
/// the parameter list. `insertArguments(pair(int, Object), 0, 1)
/// .invoke((Object) Integer.valueOf(5))` unboxed the `Integer` as `pair`'s
/// `int a` and handed `pair`'s `Object b` a raw `int`. A `type` that is missing,
/// or whose arity is not the argument count, leaves the arguments as passed:
/// the leaf arms convert them ([`adapt_leaf_args`]).
///
/// `declared` is the handle's [`mh_declared_descriptor`], which the door has
/// already read (round 12 wave 7: read once per call).
pub(crate) fn adapter_entry_args(
    ctx: &mut dyn NativeContext,
    declared: Option<&str>,
    extra: &[Value],
    call_site: Option<&str>,
) -> Result<Vec<Value>, MethodCallFailed> {
    let Some(declared) = declared else {
        return Ok(extra.to_vec());
    };
    match split_descriptor_params(declared) {
        Some((params, _)) if params.len() == extra.len() => {}
        _ => return Ok(extra.to_vec()),
    }
    let site = leaf_site_params(call_site, 0, extra.len());
    // The door's own conversion, from the call site to the handle's type, is
    // `asType`'s even when that type is an `explicitCastArguments` stamp: the
    // explicit cast is from the stamp to what lies beneath it.
    adapt_leaf_args(ctx, extra, declared, site.as_deref(), false)
}

/// JLS 5.5's primitive-to-primitive cast, as `explicitCastArguments` applies
/// it: a widening or a narrowing (`l2i`, `i2b`, `d2i`, ... through
/// [`narrow_return_value`], whose Rust `as` casts are the JVM's), and to
/// `boolean` the low-order bit of the value converted to `byte`. A `Value::Int`
/// is read as an `int` -- right for every narrower source too, since casting a
/// `short`/`char`/`byte` value to `byte`/`short`/`char` is casting its `int`
/// value. Anything else is returned as is.
pub(crate) fn explicit_cast_primitive(v: Value, pdesc: &str) -> Value {
    let Some(&to) = pdesc.as_bytes().first() else {
        return v;
    };
    if to == b'Z' {
        return match v {
            Value::Int(i) => Value::Int(i & 1),
            Value::Long(l) => Value::Int((l & 1) as i32),
            Value::Float(f) => Value::Int((f as i32) & 1),
            Value::Double(d) => Value::Int((d as i32) & 1),
            other => other,
        };
    }
    let from = match v {
        Value::Int(_) => b'I',
        Value::Long(_) => b'J',
        Value::Float(_) => b'F',
        Value::Double(_) => b'D',
        other => return other,
    };
    if primitive_widens(from, to) {
        return widen_primitive_to_descriptor(v, pdesc);
    }
    narrow_return_value(v, from, to).unwrap_or(v)
}

thread_local! {
    /// Set while [`mh_dispatch`] runs a handle marked
    /// [`MH_MARK_EXPLICIT_CAST`] and everything that handle dispatches in turn
    /// (an adapter's leaf is not itself marked; the explicit cast lies between
    /// the adapter's stamped type and the adapter). Read by the leaf arms'
    /// argument conversion ([`adapt_leaf_args`]'s `explicit`).
    ///
    /// Cleared for the extent of every door (`invoke`, `invokeExact`,
    /// `invokeWithArguments`, [`mh_dispatch_from_door`]): Java code the leaf
    /// runs reaches another handle only through a door, so the scope never
    /// crosses a Java frame. Transient per-thread dispatch state, like
    /// [`MH_ENTRY_CALL_SITE`]; a yield that unwinds the dispatch restores it
    /// on the way out.
    pub(crate) static MH_EXPLICIT_SCOPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// Armed by a door for the next [`mh_dispatch`] and TAKEN at its top
    /// ([`take_mh_tail`]): "nothing between the door and this dispatch does
    /// any work after it returns, and the door hands a REFERENCE result back
    /// unchanged". An arm whose last act is a dispatch whose result it returns
    /// as is (`INSERT`, `DROP`, `PERMUTE`, `SPREAD`, `COLLECT`, the invokers,
    /// a guard's chosen branch, the target of `filterArguments` /
    /// `foldArguments` / `collectArguments`) re-arms it for that final
    /// dispatch; every other dispatch (a guard's test, a filter, a combiner,
    /// anything under `catchException` / `filterReturnValue` / `tryFinally` /
    /// a loop) sees `0`. A leaf arm that sees it and returns a reference declares the
    /// door native TRANSPARENT for its nested Java call
    /// (`cratonvm_native_api::continuation_pin::transparent`), so a virtual
    /// thread that parks under it may unmount, as it does on HotSpot, where
    /// method handles have no native frame (wave 26, lane L4). A constructor
    /// and every arm with post-call work keep the door opaque: the
    /// continuation is pinned.
    ///
    /// Since wave 28 (lane L4, stage 1 of
    /// `docs/feature-designs/native-dispatch-architecture.md`)
    /// the value is the door's RETURN TOKEN ([`door_tail_token`]): what the
    /// door's caller receives the leaf's result as -- `b'L'` for any
    /// reference, a primitive's descriptor byte, `b'V'` -- or `0` for none.
    /// The door's own post-call work is then exactly the conversion from the
    /// leaf's return to that token, so a leaf whose conversion the remounted
    /// frame's return can make instead (a primitive boxed or widened; nothing
    /// for the same primitive or `void`) is transparent too, and records the
    /// conversion if its nested call yields ([`transparent_leaf_call`]). A
    /// `void` leaf behind a reference token (the door's `null`) still pins
    /// unless `CRATONVM_MH_VOID_TAIL_UNMOUNT` (interpreter round i6 wave 115,
    /// lane L4, default off), under which it records a `null` the
    /// interpreter's `void`-return arms push for the remounted frame.
    pub(crate) static MH_TAIL: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };

    /// The address of the `WrongMethodTypeException` a LEAF arm's arity
    /// refusal ([`leaf_short_arity_refusal`]) raised last on this thread, or
    /// `0`: a refusal that does not know the call site's type, for the door
    /// that does to reword ([`mh_deep_refusal_site_text`]; interpreter round
    /// i2 wave 54, lane L4). Compared by identity only, never dereferenced; any
    /// door's error path takes it, so a record no door claimed is dropped by
    /// the next one.
    pub(crate) static MH_DEEP_ASTYPE_REFUSAL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read and clear [`MH_TAIL`].
pub(crate) fn take_mh_tail() -> u8 {
    MH_TAIL.with(|f| f.replace(0))
}

/// Arm [`MH_TAIL`] for the dispatch made IMMEDIATELY after this call.
pub(crate) fn set_mh_tail(token: u8) {
    MH_TAIL.with(|f| f.set(token));
}

/// [`MH_TAIL`]'s token for a door whose caller receives the result as the
/// return type `ret` (a descriptor token): `b'L'` for a reference or array,
/// the primitive's (or `void`'s) descriptor byte, `0` for anything else.
pub(crate) fn door_tail_token(ret: &str) -> u8 {
    match ret.as_bytes().first() {
        Some(b'L' | b'[') => b'L',
        Some(&c @ (b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' | b'V')) => c,
        _ => 0,
    }
}

/// Whether a leaf returning `leaf` (a descriptor byte) may run transparent
/// under a door whose [`MH_TAIL`] token is `tail`, and the conversion its
/// remounted frame then owes (`Some(None)`: none; `Some(Some(token))`: to the
/// descriptor token `token`). Exactly the conversions the door would have made
/// that `interpreter::coerce_return` makes the same way: a reference or the
/// same primitive / `void` as is, a primitive boxed to a reference (its own
/// wrapper's `valueOf`, as `auto_box_return`), a primitive widened (as
/// `widen_return_value`). `None` for the rest (a `void` leaf behind a value,
/// a reference leaf behind a primitive, a narrowing): the door stays opaque.
pub(crate) fn tail_leaf_conversion(tail: u8, leaf: u8) -> Option<Option<&'static str>> {
    // Interpreter round i6 wave 115, lane L4 (`CRATONVM_MH_VOID_TAIL_UNMOUNT`,
    // default off): a `void` leaf behind a reference token -- the door's
    // `null` -- is transparent too, and owes the remounted frame's `void`
    // return a `null` push (`interpreter::push_void_return_across_continuation_boundary`).
    // Read only for that one pair, so every other leaf pays nothing.
    if tail == b'L' && leaf == b'V' {
        return tail_leaf_conversion_with(tail, leaf, crate::nbflags().mh_void_tail_unmount);
    }
    tail_leaf_conversion_with(tail, leaf, false)
}

/// [`tail_leaf_conversion`] with the `void`-behind-a-reference admission
/// (`CRATONVM_MH_VOID_TAIL_UNMOUNT`) stated: `void_to_reference` makes
/// `(b'L', b'V')` `Some(Some(DESC_OBJECT))` (the door's `null`, pushed by the
/// interpreter's `void`-return arms for an adapter whose caller return is a
/// reference) instead of opaque. A pure function, for the unit tests.
pub(crate) fn tail_leaf_conversion_with(
    tail: u8,
    leaf: u8,
    void_to_reference: bool,
) -> Option<Option<&'static str>> {
    if void_to_reference && tail == b'L' && leaf == b'V' {
        return Some(Some(DESC_OBJECT));
    }
    let is_reference = |c: u8| matches!(c, b'L' | b'[');
    let primitive = |c: u8| -> Option<&'static str> {
        Some(match c {
            b'Z' => DESC_BOOLEAN,
            b'B' => DESC_BYTE,
            b'C' => DESC_CHAR,
            b'S' => DESC_SHORT,
            b'I' => DESC_INT,
            b'J' => DESC_LONG,
            b'F' => DESC_FLOAT,
            b'D' => DESC_DOUBLE,
            _ => return None,
        })
    };
    if tail == 0 {
        return None;
    }
    if is_reference(leaf) {
        return (tail == b'L').then_some(None);
    }
    if tail == leaf {
        return Some(None);
    }
    primitive(leaf)?;
    if tail == b'L' {
        return Some(Some(DESC_OBJECT));
    }
    let to = primitive(tail)?;
    primitive_widens(leaf, tail).then_some(Some(to))
}

/// A leaf arm's declaration for the nested Java call it is about to make
/// ([`transparent_leaf_call`]): the transparent native frame, and the frame
/// mark and return token to record if the call yields.
pub(crate) struct TailLeafCall {
    pub(crate) _transparent: cratonvm_native_api::continuation_pin::TransparentGuard,
    pub(crate) owed: Option<(usize, &'static str)>,
    /// The leaf returns `void` behind a reference token (the door's `null`,
    /// `CRATONVM_MH_VOID_TAIL_UNMOUNT`, interpreter round i6 wave 115, lane
    /// L4): a yield is recorded through
    /// `NativeContext::note_continuation_void_leaf_null`, which also covers a
    /// native leaf that yielded itself and pushed no frame.
    pub(crate) void_leaf: bool,
}

/// A leaf arm's declaration that the door's native frame is transparent for
/// the nested Java call it is about to make ([`MH_TAIL`]), for a leaf whose
/// descriptor is `desc`; `None` when it is not. Pass the declaration to
/// [`transparent_leaf_call_done`] with the call's result.
///
/// When the remounted leaf frame will owe a conversion
/// ([`tail_leaf_conversion`]) the frame the call is about to push is marked
/// (`continuation_frame_mark`), and a context that keeps no frames keeps the
/// door opaque. `receiver`: a virtual leaf's receiver. A lambda proxy's
/// (`ClassId` in the proxy range) keeps the door opaque when a conversion is
/// owed, because the lambda door records its own conversion for the same
/// frame (`try_lambda_dispatch`) and the two would not compose.
pub(crate) fn transparent_leaf_call(
    ctx: &dyn NativeContext,
    tail: u8,
    desc: &str,
    receiver: Option<ObjectRef>,
) -> Option<TailLeafCall> {
    // Not in tail position (the common case): no descriptor walk.
    if tail == 0 {
        return None;
    }
    let leaf = *return_type_desc(desc).as_bytes().first()?;
    let owed = match tail_leaf_conversion(tail, leaf)? {
        None => None,
        Some(token) => {
            let lambda_receiver =
                receiver.is_some_and(|r| ctx.class_id_of_object(r).as_u32() >= 0x8000_0000);
            let mark = ctx.continuation_frame_mark();
            if lambda_receiver || mark == usize::MAX {
                return None;
            }
            Some((mark, token))
        }
    };
    Some(TailLeafCall {
        _transparent: cratonvm_native_api::continuation_pin::transparent(),
        owed,
        void_leaf: leaf == b'V' && owed.is_some(),
    })
}

/// End a [`transparent_leaf_call`]: withdraw the declaration and, when the
/// nested call returned a continuation yield and the remounted frame owes a
/// conversion, record it (`NativeContext::note_continuation_return`) -- the
/// lambda door's discipline: on the way out of a yield, never on a normal
/// return. `CRATONVM_DBG_MH_DISPATCH` prints each record (the positive
/// control of `L4W28VirtualMhReturnUnmount`).
pub(crate) fn transparent_leaf_call_done(
    ctx: &mut dyn NativeContext,
    call: Option<TailLeafCall>,
    result: &MethodCallResult,
) {
    let Some(call) = call else {
        return;
    };
    let owed = call.owed;
    let void_leaf = call.void_leaf;
    drop(call);
    let Some((mark, token)) = owed else {
        return;
    };
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!(
                "[MH_TAIL_ADAPTER] frame={mark} caller_return={token}{}",
                if void_leaf { " void-leaf" } else { "" }
            );
        }
        // Wave 115 (`CRATONVM_MH_VOID_TAIL_UNMOUNT`): the door's `null` for a
        // `void` leaf, whether the leaf's Java frame or a native leaf itself
        // yielded (no frame at `mark`: its caller resumes past the invoke).
        if void_leaf {
            ctx.note_continuation_void_leaf_null(mark);
        } else {
            ctx.note_continuation_return(mark, token);
        }
    }
}

/// Is the current dispatch under an `explicitCastArguments` stamp? See
/// [`MH_EXPLICIT_SCOPE`].
pub(crate) fn mh_explicit_scope() -> bool {
    MH_EXPLICIT_SCOPE.with(|f| f.get())
}

/// [`mh_dispatch`] from one of the `MethodHandle` doors: the explicit-cast
/// scope of any dispatch this door was reached from (through the Java code a
/// leaf ran) is not this call's, so it is cleared for the call and restored
/// after it. `tail`: the door's [`MH_TAIL`] token ([`door_tail_token`]; `0`
/// when the door has post-call work beyond converting the leaf's return to
/// it).
pub(crate) fn mh_dispatch_from_door(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
    tail: u8,
) -> MethodCallResult {
    let saved = MH_EXPLICIT_SCOPE.with(|f| f.replace(false));
    set_mh_tail(tail);
    let result = mh_dispatch(ctx, mh, extra_args);
    set_mh_tail(0);
    MH_EXPLICIT_SCOPE.with(|f| f.set(saved));
    result
}

/// The value an adapter passes for a slot of type `ty` that it has nothing
/// for: the primitive's zero (what HotSpot's `tryFinally` hands its cleanup
/// as the result of a target that threw, and a loop's variable with no
/// `init`), else `null`.
pub(crate) fn mh_zero_value(ty: &str) -> Value {
    if matches!(ty, "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D") {
        default_for_descriptor(ty)
    } else {
        Value::Object(None)
    }
}

/// Replicate `MethodHandle.asVarargsCollector` semantics for the
/// `invokeWithArguments` "spread" path: when the resolved target method
/// `class.name desc` is declared varargs (`ACC_VARARGS`) and the supplied
/// `params` (the method-formal arguments, with any receiver already stripped)
/// are NOT already in packed form, collect the trailing arguments into a fresh
/// array of the varargs component type.
///
/// SpEL's `FunctionReference` hands a registered varargs `MethodHandle` the
/// arguments FLAT (e.g. `#message('fmt', 'a', 'b', 'c')` →
/// `invokeWithArguments(['fmt','a','b','c'])`) and relies on varargs-collector
/// semantics to gather `'a','b','c'` into the trailing array. Without this the
/// trailing array parameter arrived null / mis-shaped
/// (VariableAndFunctionTests.functionWith{Primitive,}VarargsViaMethodHandle).
///
/// The gate is the `ACC_VARARGS` flag on the resolved target, and an
/// already-packed call (exactly N args with the last an array) is returned
/// unchanged — so non-varargs dispatch and the direct `invoke` / `invokeExact`
/// callers (which pass the array explicitly) are byte-for-byte unaffected.
///
/// `generic_collector` is the one thing this function cannot derive from the
/// target: the dispatch came through `MethodHandle.invokeWithArguments` AND
/// the handle is a varargs collector. See [`MH_GENERIC_SPREAD`] for why that
/// combination is what decides a `null` in the trailing array slot.
///
/// # Known deviation — an ALREADY-PACKED array on the generic entry
///
/// HotSpot collects unconditionally on that entry, so
/// `vf.invokeWithArguments(new Object[] {new String[0]})` throws
/// `ClassCastException: Cannot cast [Ljava.lang.String; to java.lang.String`
/// there (it wraps the array as the single ELEMENT of a new one). CratonVM
/// keeps its packed-call passthrough, which is the more forgiving answer and
/// which every in-tree caller depends on. Only the `null` half of the rule is
/// implemented here, because only the `null` half turns a working program into
/// a wrong one; the packed half turns a working program into a CCE. Measured
/// as row B06 of `probes/MhVarargsNullProbe.java`, and recorded in
/// `docs/known-issues/spring/`.
pub(crate) fn collect_trailing_varargs(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
    params: &[Value],
    generic_collector: bool,
) -> Vec<Value> {
    // Cheap pre-checks BEFORE the allocating `declared_methods` lookup, so the
    // hot path (every static/virtual MethodHandle dispatch -- Groovy/Gradle/
    // Jackson/SpEL-compiled) pays only a descriptor parse, not a full
    // declared-methods scan.
    let (ptypes, _) = crate::lang_class::parse_descriptor_param_and_return(desc);
    let p = ptypes.len();
    // Locate the array-typed parameter. Real JDK varargs requires it to be
    // the syntactically LAST parameter (JLS) -- but JRuby's Ruby-call
    // convention routinely appends a trailing `Block` parameter AFTER the
    // args array (e.g. `InvokeSite#invoke(ThreadContext, IRubyObject caller,
    // IRubyObject self, IRubyObject[] args, Block)`), so search for the
    // array anywhere in the descriptor rather than assuming index `p - 1`.
    let array_idx = match ptypes.iter().position(|t| t.starts_with('[')) {
        Some(idx) => idx,
        // No array parameter at all -- this can't be a varargs/collect
        // target, return untouched.
        None => return params.to_vec(),
    };
    let last = ptypes[array_idx].clone();
    let trailing_types = &ptypes[array_idx + 1..];
    // Does a `null` in the array slot mean "the array is null" or "one element
    // that happens to be null"? On the generic entry of a varargs collector it
    // is always the latter — and only for the SYNTACTICALLY LAST parameter,
    // which is the only one a Java collector can own (JLS §8.4.1). JRuby's
    // `(…, IRubyObject[] args, Block)` shape, which this function locates by
    // searching for the array ANYWHERE, is therefore never affected.
    let null_collects = generic_collector && array_idx + 1 == p;
    // Already packed: exactly P args and the array-position value is itself
    // an array (or null). Covers a correct `invokeExact`/pre-packed call AND
    // e.g. `#formatPrimitiveVarargs('fmt', new int[]{1})`. No collection
    // needed regardless of varargs-ness, so skip the method-table lookup
    // entirely.
    if params.len() == p {
        match params.get(array_idx) {
            Some(Value::Object(Some(arr))) if ctx.object_is_array(*arr) => return params.to_vec(),
            Some(Value::Object(None)) if !null_collects => return params.to_vec(),
            _ => {}
        }
    }
    // Confirm collection is actually warranted before reshaping the
    // arguments. Two independent triggers, either one is sufficient:
    //
    //  1. `is_varargs` -- the target is a genuine Java ACC_VARARGS method
    //     (`foo(Object... xs)`), reached via reflection/MethodHandle spread
    //     calling convention (`invokeWithArguments`, Groovy's boxed-args
    //     dispatch, ...). This was the ONLY trigger originally.
    //
    //  2. `params.len() > p` -- MORE flat argument values were supplied than
    //     the target descriptor declares params for, and SOME declared param
    //     is an array type. This covers a target method whose array
    //     parameter is an ORDINARY (non-varargs) `T[]` -- e.g. JRuby 10.x's
    //     `org.jruby.ir.targets.indy.InvokeSite`/`NormalInvokeSite
    //     .invoke(ThreadContext, IRubyObject, IRubyObject, IRubyObject[],
    //     Block)` (confirmed via `javap` -- both real overloads take a plain
    //     array, neither is declared `IRubyObject...`, so ACC_VARARGS is
    //     never set on either). JRuby's `invokebinder`-built call-site chain
    //     supplies the trailing Ruby-level arguments as flat individual
    //     values via a sequence of `MethodHandles.insertArguments` calls
    //     (CratonVM's `MH_KIND_INSERT`, which splices correctly at its own
    //     `pos` -- verified by direct value tracing, not the bug) and never
    //     calls `MethodHandle.asCollector`/anything else that would pack
    //     them -- so by the time dispatch reaches the target method's own
    //     descriptor, arity strictly exceeds the declared param count with
    //     an array type declared somewhere in it. In a signature-polymorphic
    //     MethodHandle-mediated call this arity/type mismatch has exactly
    //     one legal resolution (collect the excess into the array); passing
    //     the excess through flat/unchanged (the old behavior) desyncs
    //     every argument at and after the array position -- confirmed via
    //     `CRATONVM_DBG_MH_DISPATCH` live tracing on
    //     `JRubyScriptTemplateTests`/`rubygems/version.rb`'s
    //     `@version.sub(regex, "")`: the terminal `NormalInvokeSite.invoke`
    //     dispatch received 6 flat args `[ctx, self, receiver, regex, BLOCK,
    //     replacement]` against a 5-param `(ctx, self, receiver, args[],
    //     block)` target -- the block landed in the array's slot, one
    //     position early, pushing the real last argument out past it.
    let cid = match ctx.class_id_by_name(class) {
        Some(c) => c,
        None => return params.to_vec(),
    };
    let is_varargs = ctx
        .declared_methods(cid)
        .iter()
        .any(|m| m.name == name && m.descriptor == desc && (m.access_flags & 0x0080) != 0);
    let arity_excess = params.len() > p;
    // A second, narrower trigger alongside `arity_excess`: EXACTLY `p` args
    // were supplied (no excess) but the value that naively lands at the
    // array's declared position isn't itself an array (or null) -- a bare
    // scalar sitting in an array-typed descriptor slot. Confirmed via the
    // SAME `JRubyScriptTemplateTests` trace as `arity_excess` above: right
    // after the `.sub()` call's 6-flat-args case (fixed by `arity_excess`),
    // the very next call in the same chain --
    // `org.jruby.ir.targets.indy.SelfInvokeSite.invoke(ThreadContext,
    // IRubyObject, IRubyObject[], Block)` -- arrived with exactly 4 flat
    // args (matching `p` exactly) where the array-typed 3rd param held a
    // single bare `IRubyObject` instead of a 1-element array, later
    // surfacing as `ArgumentError: wrong number of arguments (given 0,
    // expected 1)` inside the interpreted Ruby method `checkArity` found it
    // was calling. A single supplied value destined for a 1-element array
    // needs the exact same wrap-into-array treatment as an excess of
    // supplied values, just with `excess == 0`.
    let array_slot_is_wrapped = match params.get(array_idx) {
        Some(Value::Object(Some(arr))) => ctx.object_is_array(*arr),
        Some(Value::Object(None)) => !null_collects,
        _ => false,
    };
    // Only a scalar that IS an element of the array wraps (wave 27): JRuby's
    // bare `IRubyObject` in an `IRubyObject[]` slot is one. An `Integer` in a
    // `String[]` slot is not, and wrapping it built a `String[]` holding an
    // `Integer` (`dropArguments(countStrings, 0, int).invokeWithArguments(1,
    // 3)` answered 1 where HotSpot throws `Cannot cast java.lang.Integer to
    // [Ljava.lang.String;`); left as passed, the leaf's cast refuses it
    // (`leaf_reference_refusal`). A primitive component keeps the old wrap.
    let scalar_needs_wrap = params.len() == p
        && !array_slot_is_wrapped
        && params.get(array_idx).is_none_or(|v| {
            let component = &last[1..];
            !matches!(component.as_bytes().first(), Some(b'L' | b'['))
                || leaf_reference_refusal(ctx, *v, component, false).is_none()
        });
    if !is_varargs && !arity_excess && !scalar_needs_wrap {
        return params.to_vec();
    }
    if params.len() < array_idx + trailing_types.len() {
        // Fewer args than the leading-fixed + trailing-fixed params could
        // ever accommodate -- let `invoke` surface the arity error rather
        // than fabricate a result.
        return params.to_vec();
    }
    // The tail region (`params[array_idx..]`) needs to split into "values
    // that collect into the array" and "values that satisfy the trailing
    // fixed params declared AFTER the array" (e.g. a trailing `Block`). A
    // naive positional split (last N values = trailing fixed params) is
    // WRONG here: those trailing values can end up spliced into the MIDDLE
    // of the tail region by an earlier, independently-correct
    // `MethodHandles.insertArguments` step whose `pos` was computed against
    // the COLLAPSED arity it expected -- CratonVM never actually collapses
    // until this function runs, so a value meant to land after the array
    // ends up interleaved among the to-be-collected values instead (exactly
    // the `[..., Regexp, Block, replacement]` shape traced above: `Block`
    // sitting between the two values that belong in the array). Recover the
    // correct split by matching each trailing declared type against its
    // RUNTIME class within the tail, pulling matched values out (in the
    // trailing params' declared order) and leaving the rest, in their
    // original relative order, to collect into the array.
    let tail = &params[array_idx..];
    let mut taken = vec![false; tail.len()];
    let mut trailing_values: Vec<Value> = Vec::with_capacity(trailing_types.len());
    for tt in trailing_types {
        let want_class = tt
            .strip_prefix('L')
            .and_then(|t| t.strip_suffix(';'))
            .unwrap_or(tt);
        let found = tail.iter().enumerate().find(|(i, v)| {
            !taken[*i]
                && matches!(v, Value::Object(Some(o))
                    // An array's header names its COMPONENT: an `X[]` is not an `X`.
                    if !matches!(ctx.heap_kind_of(*o), ObjectKind::Array)
                        && ctx.class_name_arc_of_id(ctx.class_id_of_object(*o)).as_deref() == Some(want_class))
        }).map(|(i, _)| i);
        let idx = found.or_else(|| (0..tail.len()).rev().find(|i| !taken[*i]));
        if let Some(i) = idx {
            taken[i] = true;
            trailing_values.push(tail[i]);
        }
    }
    let collected: Vec<Value> = tail
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken[*i])
        .map(|(_, v)| *v)
        .collect();
    let component = &last[1..]; // strip one leading '['
                                // Round 15 wave 6 (lane mh5): the values around the array are read back
                                // through pins after its allocation (see `build_varargs_array`), not
                                // copied from their pre-allocation addresses. `lead_base` (lowest)
                                // releases both groups.
    let (lead_base, lead) = pin_mh_args(ctx, &params[..array_idx]);
    let (trail_base, trail) = pin_mh_args(ctx, &trailing_values);
    let array = build_varargs_array(ctx, component, &collected, None, 0);
    let mut out = Vec::with_capacity(array_idx + 1 + trailing_values.len());
    out.extend(read_pinned_mh_args(ctx, &lead, &params[..array_idx]));
    out.push(Value::Object(array));
    out.extend(read_pinned_mh_args(ctx, &trail, &trailing_values));
    let first = if lead_base != usize::MAX {
        lead_base
    } else {
        trail_base
    };
    if first != usize::MAX {
        ctx.unpin_native_roots(first);
    }
    out
}

/// Coerce a value to an `i32` for a category-1 primitive varargs element
/// (int/short/byte/char/boolean), unboxing a wrapper if needed.
pub(crate) fn varargs_coerce_int(ctx: &mut dyn NativeContext, v: Value) -> i32 {
    match v {
        Value::Int(x) => x,
        Value::Long(x) => x as i32,
        Value::Float(x) => x as i32,
        Value::Double(x) => x as i32,
        Value::Object(Some(o)) => match crate::lang_class::unbox_value(ctx, o) {
            Value::Int(x) => x,
            Value::Long(x) => x as i32,
            Value::Float(x) => x as i32,
            Value::Double(x) => x as i32,
            _ => 0,
        },
        _ => 0,
    }
}

/// Allocate the varargs array of JVM type-descriptor `component` holding `vals`,
/// converting each element to the component type (unbox wrappers for a primitive
/// component; box raw primitives for a reference component).
///
/// `call_site` and `start_index` are [`collector_call_site_prim`]'s inputs for
/// each element of `vals` (element `i` sits at call-site position
/// `start_index + i`) — `None`/`0` from [`collect_trailing_varargs`]'s
/// permissive path, which has no call site to offer; `Some`/the collected
/// region's own offset from [`mh_entry_adapt`], which does. F29-1 NOMINATION
/// 2: `var.invoke(aChar)` on an `Object[]`-collecting varargs handle is
/// boxed by the CALL SITE the caller wrote, same as `MH_KIND_COLLECT`
/// (MEASURED `mhvar.char` = `java.lang.Character` on HotSpot 25).
pub(crate) fn build_varargs_array(
    ctx: &mut dyn NativeContext,
    component: &str,
    vals: &[Value],
    call_site: Option<&str>,
    start_index: usize,
) -> Option<ObjectRef> {
    use cratonvm_types::ArrayElementType as ET;
    let n = vals.len();
    // Round 15 wave 6 (lane mh5): every arm allocates (the array, and for a
    // reference component a box per primitive element) BEFORE or between its
    // reads of `vals`, whose references a moving collection relocates: a
    // wrapper was unboxed, or an element stored, from its pre-collection
    // address. Each is pinned here and read back through its pin.
    let (vals_base, handles) = pin_mh_args(ctx, vals);
    let val = |ctx: &dyn NativeContext, i: usize| read_pinned_mh_arg(ctx, handles[i], vals[i]);
    let built = match component {
        "I" | "S" | "B" | "C" | "Z" => {
            let et = match component {
                "I" => ET::Int,
                "S" => ET::Short,
                "B" => ET::Byte,
                "C" => ET::Char,
                _ => ET::Boolean,
            };
            let arr = ctx.new_array(et, n);
            for i in 0..n {
                let v = val(&*ctx, i);
                let iv = varargs_coerce_int(ctx, v);
                ctx.set_array_element(arr, i, Value::Int(iv));
            }
            Some(arr)
        }
        "J" => {
            let arr = ctx.new_array(ET::Long, n);
            for i in 0..n {
                let v = &val(&*ctx, i);
                let lv = match *v {
                    Value::Long(x) => x,
                    Value::Int(x) => x as i64,
                    Value::Object(Some(o)) => match crate::lang_class::unbox_value(ctx, o) {
                        Value::Long(x) => x,
                        Value::Int(x) => x as i64,
                        _ => 0,
                    },
                    _ => 0,
                };
                ctx.set_array_element(arr, i, Value::Long(lv));
            }
            Some(arr)
        }
        "F" => {
            let arr = ctx.new_array(ET::Float, n);
            for i in 0..n {
                let v = &val(&*ctx, i);
                let fv = match *v {
                    Value::Float(x) => x,
                    Value::Int(x) => x as f32,
                    Value::Object(Some(o)) => match crate::lang_class::unbox_value(ctx, o) {
                        Value::Float(x) => x,
                        Value::Int(x) => x as f32,
                        _ => 0.0,
                    },
                    _ => 0.0,
                };
                ctx.set_array_element(arr, i, Value::Float(fv));
            }
            Some(arr)
        }
        "D" => {
            let arr = ctx.new_array(ET::Double, n);
            for i in 0..n {
                let v = &val(&*ctx, i);
                let dv = match *v {
                    Value::Double(x) => x,
                    Value::Float(x) => x as f64,
                    Value::Int(x) => x as f64,
                    Value::Long(x) => x as f64,
                    Value::Object(Some(o)) => match crate::lang_class::unbox_value(ctx, o) {
                        Value::Double(x) => x,
                        Value::Float(x) => x as f64,
                        Value::Int(x) => x as f64,
                        Value::Long(x) => x as f64,
                        _ => 0.0,
                    },
                    _ => 0.0,
                };
                ctx.set_array_element(arr, i, Value::Double(dv));
            }
            Some(arr)
        }
        _ => {
            // Reference component: `Ljava/lang/Object;`, `Ljava/lang/String;`,
            // or a nested array descriptor like `[I`.
            let comp_name = if component.starts_with('L') && component.ends_with(';') {
                &component[1..component.len() - 1]
            } else {
                component
            };
            let comp_id = ctx
                .ensure_class_initialized(comp_name)
                .unwrap_or(cratonvm_types::ClassId::new(0));
            let arr = ctx.new_ref_array(comp_id, n);
            // Pin the array across the boxing loop: `box_value` allocates and may
            // relocate `arr` under a moving collector.
            let pin = ctx.pin_native_root(arr);
            let mut arr = arr;
            for i in 0..n {
                let v = &val(&*ctx, i);
                // The varargs twin of `mh_dispatch`'s collector loop, and now
                // literally the same function rather than a copy of it — the
                // two arms disagreeing is the shape that produced this
                // family's defects. `component` here is the TARGET's trailing
                // array component, so `vChar(Character...)` settles the
                // wrapper class the same way `asCollector(Character[], 1)`
                // does: MEASURED `H.vChar.fromChar` = `java.lang.Character`,
                // `H.vChar.id` = true, and `H.vLong.fromInt` throws on
                // HotSpot rather than widening. Measured `mhvar.int` = true.
                let ov = box_collector_element(
                    ctx,
                    *v,
                    component,
                    collector_call_site_prim(call_site, start_index + i),
                );
                arr = ctx.read_native_pin(pin, arr);
                ctx.set_array_element(arr, i, ov);
            }
            ctx.unpin_native_roots(pin);
            Some(arr)
        }
    };
    if vals_base != usize::MAX {
        ctx.unpin_native_roots(vals_base);
    }
    built
}

/// Extract the return type descriptor from a method descriptor.
/// e.g. "(II)I" → "I", "(Ljava/lang/String;)V" → "V"
// ---------------------------------------------------------------------------
// `invokeExact` -- the CALL SITE must match the handle's type exactly
// ---------------------------------------------------------------------------

/// Kill switch for the two rules below. Deliberately the SAME declared name the
/// return-value half already uses (`vm_exec::mh_strict_invokeexact`,
/// `CRATONVM_COMPAT=mh-strict-invokeexact`), because it is the same rule seen
/// from the other end -- one switch turns `invokeExact` strictness off wherever
/// it is enforced. Set the exact string `0` to restore the pre-2026-08-28
/// permissive dispatch.
///
/// Latches, like every declared flag: exporting the variable before launching
/// the process is the only way to set it.
///
/// **What it does NOT revert, measured rather than assumed.** Setting it to `0`
/// restores every REFUSAL to the permissive answer — `invokeExact` accepts a
/// mismatched call site again, a null receiver answers `0` again, `invoke`
/// accepts a narrowing argument again. It does NOT revert
/// [`box_return_against_target`], and deliberately: `(long) max.invoke(1, 2)`
/// answers `2` with the switch off as well as on. That is a wrong VALUE being
/// corrected, not a rule being enforced, and there is no state of the world in
/// which a caller wants the fabricated zero back. A kill switch for a
/// strictness rule should not carry an unrelated correction out with it.
pub(crate) fn mh_strict_exact_signature() -> bool {
    static STRICT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STRICT.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_MH_STRICT_INVOKEEXACT").as_deref(),
            Ok("0")
        )
    })
}

/// The `MH_KIND_*` values whose `type` field is written from a resolved
/// member's OWN descriptor, and is therefore an authoritative statement of what
/// the handle's signature is.
///
/// **This allowlist is the whole safety argument, so it is worth being exact
/// about what the recorded objection did and did not say.** The `invokeExact`
/// registration below has carried a warning since a strict ARITY check there
/// aborted the VM and killed every Groovy `IndyInterface` call site
/// ("expected 2 args, got 1"). That check read `MH_DESC` -- the raw bytecode
/// descriptor -- and `MH_DESC` is genuinely unusable for this: the adapter
/// kinds keep their inner target's, and it omits the receiver that a virtual
/// handle's `type` prepends. The check below reads `type` instead, which
/// `mh_type_descriptor` documents as the ADAPTED MethodType that "combinators
/// must chain off so a stack of adapters tracks arity correctly".
///
/// That is a good reason to believe the adapters would survive a check too. It
/// is not a measurement, and the failure mode on the other side is a refusal of
/// working code on Groovy's hot path, so the adapters stay out until someone
/// runs them. What is in: the six kinds `finish_method_handle` builds, with the
/// two HotSpot adjustments it applies (receiver prepended for virtual/special,
/// the constructed class as a constructor's return). `bindTo` and `asType`
/// preserve the direct kind AND restamp `type`, so a bound or retyped direct
/// handle IS checked -- against its restamped signature, which is the correct
/// one.
///
/// **The field accessors are IN, and it took a measurement to say so.** I first
/// excluded `MH_KIND_GETTER`/`MH_KIND_SETTER` after reading
/// `finish_method_handle`'s own comment -- "STATIC/CONSTRUCTOR/GETTER keep raw
/// `desc`", where VIRTUAL and SPECIAL get the receiver prepended -- and
/// concluding that an instance `findGetter` handle must report `()int` where
/// HotSpot reports `(Box)int`, which would make this check refuse the correct
/// call at `regression-suite/src/RJdkHandles.java:159`.
///
/// It does not. `findGetter`'s raw `desc` ALREADY names the receiver, so there
/// is nothing to prepend, and CratonVM renders `(Box)int` / `(Box,int)void` /
/// `()int` for an instance getter, an instance setter and a static getter --
/// byte-identical to the oracle (`probes/L5ModuleInvokeSweep.java`,
/// `ie.type.getter` / `ie.type.setter` / `ie.type.sget`). The comment is about
/// a prepend that would be redundant, not about a signature that is short.
/// Reading it as a claim about `type` cost two rows the check can make.
///
/// **Do not widen this one to the combinator surface.** This predicate also
/// gates [`invoke_reference_cast_refusal`], which reads `MH_DESC` -- the RAW
/// leaf descriptor, by that function's own comment a few lines below -- not
/// `type`. An adapter's `MH_DESC` is its LEAF member's descriptor, not its
/// adapted one (`filterArguments(cat, 1, intToString)` keeps `cat`'s `MH_DESC`
/// while presenting a different parameter list), so adding adapter kinds HERE
/// would make that function compare a caller's argument against the wrong
/// signature and refuse correct calls. [`kind_has_authoritative_adapted_type`]
/// is the measured, `type`-reading counterpart for the two checks that can
/// safely include them.
pub(crate) fn kind_has_authoritative_type(kind: i32) -> bool {
    matches!(
        kind,
        MH_KIND_STATIC
            | MH_KIND_VIRTUAL
            | MH_KIND_SPECIAL
            | MH_KIND_CONSTRUCTOR
            | MH_KIND_GETTER
            | MH_KIND_SETTER
    )
}

/// [`kind_has_authoritative_type`], widened to the `MethodHandles` combinator
/// surface, for the two checks that read `type` (via [`mh_declared_descriptor`])
/// rather than `MH_DESC`: `exact_call_site_refusal`'s strict `invokeExact`
/// signature check and `invoke_narrowing_arg_refusal`'s primitive-narrowing
/// refusal. `invoke_reference_cast_refusal` stays on the narrower predicate --
/// see its comment there for why `MH_DESC` makes the adapters unsafe for THAT
/// check specifically.
///
/// **The `MethodHandles` combinator surface is IN, now that it has been run.**
/// `MhCombinatorSweep.java` swept 56 rows of exactly this surface through
/// `invoke` and came back byte-identical to HotSpot in both modes
/// (`methodhandles-combinator-surface-16-of-56-RETIRED-20260917.md`), which
/// establishes the adapted `type()` VALUE is right for every kind below.
/// `MhAdapterInvokeExactSweep.java` is the measurement `kind_has_authoritative_
/// type`'s comment said nobody had run: the same combinator shapes, called
/// through `invokeExact` instead of `invoke`, both on a matching call site
/// (must still dispatch and answer the same value) and a deliberately
/// mismatched one (must now throw `WrongMethodTypeException`, byte-identical
/// to HotSpot). Clean in both modes, plus `CodegenFrameworkSmoke.java`'s
/// Groovy rows (closures compile through `IndyInterface`, which stacks
/// `insert` / `asType` / `asCollector` on real call sites), the Netty and
/// Spring corpora, and the three `regression-suite` arms, all unaffected --
/// see the retired L5-residuals write-up for the full run.
///
/// What is added: the eleven kinds `register_method_handle_combinator_extras_
/// bridge` and `MethodHandles.permuteArguments`/`collectArguments` build, each
/// of which either computes its `type` from the target's own ADAPTED
/// descriptor before `alloc_method_handle` (`FILTER`, `RETURN_FILTER`,
/// `COLLECT_ARGS`, `DROP`, `GUARD`, `PERMUTE`) or overwrites `type` right after
/// allocation with a freshly built adapted `MethodType` (`INSERT`, `COLLECT`,
/// `SPREAD`, `FOLD`, `CATCH`) -- never the leaf's raw `MH_DESC`, which is what
/// made the original ARITY check unsafe. `TRY_FINALLY` and the `LOOP_*` family
/// are deliberately still OUT: neither sweep above exercises them through
/// `invokeExact`, so there is no measurement backing them yet, and
/// `MhAdapterInvokeExactSweep`'s two `[CratonVM-only]` rows exist to catch this
/// list growing to include them by accident. Both checks that use this
/// predicate stay behind `CRATONVM_MH_STRICT_INVOKEEXACT`, so a wrong row in
/// either reverts with the existing kill switch -- no new one was added for
/// this widening.
///
/// Round 15 wave 6 (lane mh5; `r15w5-mh4-review-findings` item 5, proposal
/// MH4-2): `TRY_FINALLY` and the four `LOOP_*` kinds are admitted, by
/// reading, behind `CRATONVM_MH_LOOP_KINDS_TYPED` (default ON; read only for
/// a handle of those five kinds). Each is minted by
/// `register_method_handle_loop_bridge` with `MH_DESC` == `type` == the JDK's
/// adapted type: `tryFinally` the target's own `type` (`tryFinally`'s result
/// type is `target.type()`); `whileLoop` / `doWhileLoop` the body's
/// parameters after its loop variable (`whileLoopChecks` holds `init` and
/// `pred` to a PREFIX of exactly that list, so the body's is the longest and
/// the loop's); `countedLoop` `mh_counted_loop_checks`' outer list (the body's
/// after `(V, int)`, else the iteration handles'); `iteratedLoop` the body's
/// after `(V, T)`, else the iterator's parameters, else `Iterable`
/// (`iteratedLoopChecks`' rule). `MhAdapterInvokeExactSweep`'s two
/// `[CratonVM-only]` rows that guarded the list are expected to flip to
/// HotSpot's answers with the switch on.
pub(crate) fn kind_has_authoritative_adapted_type(kind: i32) -> bool {
    kind_has_authoritative_type(kind)
        || matches!(
            kind,
            MH_KIND_INSERT
                | MH_KIND_COLLECT
                | MH_KIND_SPREAD
                | MH_KIND_DROP
                | MH_KIND_GUARD
                | MH_KIND_FILTER
                | MH_KIND_RETURN_FILTER
                | MH_KIND_FOLD
                | MH_KIND_CATCH
                | MH_KIND_PERMUTE
                | MH_KIND_COLLECT_ARGS
                | MH_KIND_TABLE_SWITCH
        )
        || (matches!(
            kind,
            MH_KIND_TRY_FINALLY
                | MH_KIND_LOOP_COUNTED
                | MH_KIND_LOOP_WHILE
                | MH_KIND_LOOP_DOWHILE
                | MH_KIND_LOOP_ITERATE
        ) && mh_loop_kinds_typed_enabled())
}

/// `CRATONVM_MH_LOOP_KINDS_TYPED` -- default ON (round 15 wave 6, lane mh5;
/// MH4-2). `0`/`false`/`off`/`no` keeps `TRY_FINALLY` and the `LOOP_*` kinds
/// out of [`kind_has_authoritative_adapted_type`] (no `invokeExact` signature
/// check, no `invoke` arity or narrowing refusal, no `type`-aligned entry
/// conversion for them), as before. Both modes. Read only for those kinds.
pub(crate) fn mh_loop_kinds_typed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_LOOP_KINDS_TYPED")
}

/// The handle's declared signature, read from the `type` field ONLY.
///
/// Never `mh_type_descriptor`, whose `MH_DESC` fallback would hand back a
/// virtual handle's receiver-less descriptor and make every correct call look
/// like an arity mismatch. Same rule as `bindTo`'s leading-parameter guard: no
/// `type`, or one whose mirrors do not render, means the signature is UNKNOWN,
/// and an unknown signature is an accept.
pub(crate) fn mh_declared_descriptor(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if crate::nbflags().mh_return_plan {
        return mh_return_facts_declared(ctx, mh).map(|d| String::from(&*d));
    }
    mh_declared_descriptor_rendered(ctx, mh)
}

/// [`mh_declared_descriptor`]'s render: the `type` field's `MethodType`.
pub(crate) fn mh_declared_descriptor_rendered(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    match ctx.get_field_by_name(mh, "type") {
        Value::Object(Some(mt)) => methodtype_to_descriptor(ctx, mt),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// A handle's return facts, memoised (interpreter round i2 wave 59, lane L4;
// proposal `i58-L4-proposal-one-return-plan-for-every-method-handle-door`,
// `CRATONVM_MH_RETURN_PLAN`)
// ---------------------------------------------------------------------------
//
// What a door decides its return conversion from ([`DoorReturn`]) is fixed per
// handle: its declared `type` (rendered from the `MethodType`'s mirrors, a
// by-name field read and a name per parameter) and, for a stamp over a stamp
// or one stamp over an adapter (`CRATONVM_MH_ADAPTER_STAMP_CONVERTS`), its
// parsed cast chain ([`MH_CAST_CHAIN`]'s text, read into a `String` and
// parsed on EVERY dispatch -- Groovy's `IndyInterface` hot path since wave
// 58). Both are written only while the handle is minted, before any reader
// has it, so within one collection epoch -- where nothing moves or is freed,
// and an equal address is the same handle -- the first answer is every
// answer: [`TYPE_RENDER_MEMO`]'s rule, keyed by the HANDLE.

/// One handle's memoised facts; each is filled the first time it is asked.
#[derive(Default)]
pub(crate) struct MhReturnFacts {
    /// [`mh_declared_descriptor_rendered`]'s answer, once asked.
    pub(crate) declared: Option<Option<std::rc::Rc<str>>>,
    /// The parsed [`MH_CAST_CHAIN`], once asked.
    pub(crate) chain: Option<Option<std::rc::Rc<CastChain>>>,
}

/// One direct-mapped line of [`MH_RETURN_FACTS_MEMO`].
pub(crate) struct MhReturnFactsLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) mh: usize,
    pub(crate) facts: MhReturnFacts,
}

/// Slots in [`MH_RETURN_FACTS_MEMO`]; a power of two (the index is a mask).
pub(crate) const MH_RETURN_FACTS_SLOTS: usize = 64;

thread_local! {
    /// Per-thread memo of [`MhReturnFacts`], keyed by the VM, the collection
    /// epoch and the handle's address (see the section comment). A context
    /// reporting VM identity `0` (the mocks) is never memoised.
    pub(crate) static MH_RETURN_FACTS_MEMO: std::cell::RefCell<Vec<MhReturnFactsLine>> =
        std::cell::RefCell::new(
            (0..MH_RETURN_FACTS_SLOTS)
                .map(|_| MhReturnFactsLine { vm: 0, epoch: 0, mh: 0, facts: MhReturnFacts::default() })
                .collect(),
        );
}

/// The memo line of `mh` under (`vm`, `epoch`), handed to `read`, which
/// answers `Some` when the line already knows; on a miss `compute` runs (with
/// the epoch read BEFORE it, so a computation that collected is stored under
/// an epoch no later lookup reads) and `store` files its answer.
pub(crate) fn mh_return_facts<T: Clone>(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    read: impl Fn(&MhReturnFacts) -> Option<T>,
    compute: impl FnOnce(&mut dyn NativeContext) -> T,
    store: impl FnOnce(&mut MhReturnFacts, T),
) -> T {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return compute(ctx);
    }
    let epoch = ctx.gc_address_epoch();
    let addr = mh.as_ptr() as usize;
    let slot = (addr >> 3) & (MH_RETURN_FACTS_SLOTS - 1);
    let hit = MH_RETURN_FACTS_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm && line.epoch == epoch && line.mh == addr {
            read(&line.facts)
        } else {
            None
        }
    });
    if let Some(hit) = hit {
        // The positive control (`CRATONVM_DBG_MH_DISPATCH`).
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!("[MH_RETURN_PLAN] hit {addr:#x}");
        }
        return hit;
    }
    let answer = compute(ctx);
    MH_RETURN_FACTS_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let line = &mut memo[slot];
        if !(line.vm == vm && line.epoch == epoch && line.mh == addr) {
            *line = MhReturnFactsLine {
                vm,
                epoch,
                mh: addr,
                facts: MhReturnFacts::default(),
            };
        }
        store(&mut line.facts, answer.clone());
    });
    answer
}

/// `mh`'s declared descriptor through [`MH_RETURN_FACTS_MEMO`].
pub(crate) fn mh_return_facts_declared(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<std::rc::Rc<str>> {
    mh_return_facts(
        ctx,
        mh,
        |facts| facts.declared.clone(),
        |ctx| mh_declared_descriptor_rendered(ctx, mh).map(|d| std::rc::Rc::from(d.as_str())),
        |facts, declared| facts.declared = Some(declared),
    )
}

/// `mh`'s parsed [`MH_CAST_CHAIN`] through [`MH_RETURN_FACTS_MEMO`]: no text
/// read and no parse on a hit.
pub(crate) fn mh_return_facts_chain(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<std::rc::Rc<CastChain>> {
    mh_return_facts(
        ctx,
        mh,
        |facts| facts.chain.clone(),
        |ctx| {
            mh_cast_chain_text(&*ctx, mh)
                .and_then(|text| parse_cast_chain(&text))
                .map(std::rc::Rc::new)
        },
        |facts, chain| facts.chain = Some(chain),
    )
}

/// The `NullPointerException` an UNBOUND virtual or special handle must raise
/// when its receiver argument is null, or `None`.
///
/// `findVirtual(String.class, "length", ()int).invokeExact((String) null)`
/// answers `0` here and throws NPE on HotSpot -- and `0` is `length()`'s
/// ordinary answer for the empty string, so the caller cannot tell the failed
/// call from a real one. Same shape as the `Field.getAnnotation(null)` defect
/// in this campaign: a null answer that is ALSO the method's legitimate answer.
///
/// Restricted to `MH_KIND_VIRTUAL`/`MH_KIND_SPECIAL` with no `MH_BOUND`, which
/// is exactly the `needs_receiver && !has_bound` condition `invoke` already
/// uses to decide that `extra[0]` is a receiver and not a descriptor
/// parameter. A GETTER/SETTER is deliberately out: those kinds cover the
/// STATIC field accessors too, and this cannot tell which from the kind alone.
pub(crate) fn null_receiver_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    extra: &[Value],
) -> Option<MethodCallFailed> {
    if !mh_strict_exact_signature() {
        return None;
    }
    if !matches!(kind, MH_KIND_VIRTUAL | MH_KIND_SPECIAL) {
        return None;
    }
    if mh_has_bound_guarded(&*ctx, mh) {
        return None;
    }
    if !matches!(extra.first(), Some(Value::Object(None))) {
        return None;
    }
    if let Some(npe) = vh_invoker_null_handle(&*ctx, mh) {
        return Some(npe);
    }
    // Message-less, as HotSpot's (`checkReceiver`'s `Objects.requireNonNull`;
    // measured on 25.0.3 for `findVirtual(String, "length", ()int)
    // .invokeExact((String) null)` and `.invoke((Object) null)`, wave 27). This
    // said "MethodHandle receiver is null for java.lang.String.length", which a
    // caller reading `getMessage()` saw where HotSpot answers `null`.
    Some(RuntimeError::NullPointerException { message: None }.into())
}

/// `CRATONVM_MH_WMTE_JDK_FRAMES` -- default ON (JIT round 16 wave 13, lane
/// wmte; a correctness fix, both modes): a door's `WrongMethodTypeException`
/// ([`mh_door_wmte`]) lists the JDK frames HotSpot raises it in
/// (`Invokers.checkExactType` / `newWrongMethodTypeException`, or `asType` /
/// `setAsTypeCache` / `asTypeUncached`), where its trace started at the
/// caller. `0`/`false`/`off`/`no` leaves the caller on top, as before. Read
/// only on a refusal path.
pub(crate) fn mh_wmte_jdk_frames_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_WMTE_JDK_FRAMES")
}

/// The `WrongMethodTypeException` (`message`) a `MethodHandle` door raises in
/// place of the JDK method `raise` names (JIT round 16 wave 13, lane wmte;
/// `docs/feature-designs/native-dispatch-architecture.md`).
/// The door pushes no frame, so HotSpot's trace (`R16MhWmteFrames`)
/// `Invokers.newWrongMethodTypeException | Invokers.checkExactType | <caller>`
/// for `invokeExact`, and `MethodHandle.asTypeUncached | setAsTypeCache |
/// asType | ..` for `invoke` / `invokeWithArguments` / `asType`, started at
/// the caller here. The VM's capture rebuilds those frames from the real class
/// bytes when told which raise this is
/// (`NativeThreadAccess::set_mh_wmte_raise_hint`): the hint is set right
/// before the exception's constructor runs (which fills the trace) and
/// withdrawn after it, so nothing else takes it. Only the messages HotSpot
/// builds in those two methods come here (`handle's method type .. but found
/// ..`, `cannot convert MethodHandle.. to ..`); a `VarHandle` refusal and
/// `AsVarargsCollector`'s `cannot build collector` keep the plain throw.
pub(crate) fn mh_door_wmte(
    ctx: &mut dyn NativeContext,
    raise: MhWmteRaise,
    message: &str,
) -> MethodCallFailed {
    let hinted = mh_wmte_jdk_frames_enabled();
    if hinted {
        ctx.set_mh_wmte_raise_hint(Some(raise));
    }
    let refusal = crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        message,
    );
    if hinted {
        ctx.set_mh_wmte_raise_hint(None);
    }
    refusal
}

/// The `WrongMethodTypeException` `mh.invokeExact(...)` must raise when the
/// call site's symbolic descriptor is not IDENTICAL to the handle's type, or
/// `None`.
///
/// `invokeExact` performs no conversion at all -- no widening, no boxing, no
/// `asType` (JLS 15.12.3, `java.lang.invoke.MethodHandle`). `invoke` is the
/// opposite and must never reach here; `invokeBasic` and the `VarHandle`
/// accessors are likewise permissive.
///
/// Measured against jdk-25.0.3.9-hotspot (`probes/L5ModuleInvokeSweep.java`):
/// before this, `findStatic(Math,"max",(int,int)int).invokeExact` accepted
/// `(long,long)`, `(short,short)`, `(byte,byte)`, `(char,char)`,
/// `(Integer,Integer)` and `(int,long)` arguments, an `Object`/`Integer`/`void`
/// return, and -- the shape hardest to explain away -- ONE argument and THREE,
/// answering `1` and `2` rather than throwing. A handle for `String.length()`
/// answered 4 for an `(Object)` and a `(CharSequence)` receiver. Every one of
/// those is the exact case `invokeExact` exists to refuse: a caller reaches for
/// it over `invoke` precisely to be told when the signature has drifted.
///
/// Every way of not knowing is an accept -- unknown kind, no `type`, either
/// descriptor unparseable, either signature unrenderable.
///
/// `MH_KIND_CONSTANT` / `MH_KIND_IDENTITY` (`constant`, `zero`, `empty`,
/// `identity`, the uninitialized call site's placeholder) are judged too
/// (interpreter round i1 wave 40, lane L4;
/// `tools/probes/interp/L4/L4W40IdentityConstantInvokeExact.java`): each is
/// minted with its exact `type` (`()T`, `(T)T`, the `empty` / placeholder
/// `MethodType` itself), an `asType` copy carries the stamped one, and
/// `bindTo` drops the bound parameter from it as HotSpot does. Groovy's two
/// `invokeExact` sites (`IndyInterface.fromCache` / `fromCacheHandle`, read
/// from the 4.0.32 and 5.0.2 jars) call `handle.asSpreader(..).asType(([Object)
/// Object)`, whose type IS the site's, so they cannot be refused here.
///
/// The array accessors (`arrayElementGetter` / `arrayElementSetter` /
/// `arrayLength`, `MH_KIND_ARRAY_*`) likewise: minted with their exact
/// `(T[],int)T` / `(T[],int,T)void` / `(T[])int`, and `bindTo` drops the
/// array (same wave, same probe, `array-*` rows).
pub(crate) fn exact_call_site_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    call_site: &str,
    declared: Option<&str>,
) -> Option<MethodCallFailed> {
    if !mh_strict_exact_signature()
        || !(kind_has_authoritative_adapted_type(kind)
            || matches!(
                kind,
                MH_KIND_CONSTANT
                    | MH_KIND_IDENTITY
                    | MH_KIND_ARRAY_GET
                    | MH_KIND_ARRAY_SET
                    | MH_KIND_ARRAY_LENGTH
            )
            || invoker_has_own_type(&*ctx, mh, kind)
            // Wave 59 (lane L4): a dynamic invoker typed by its call site
            // (`CRATONVM_MH_DYNAMIC_INVOKER_SITE_TYPE`) is exact as HotSpot's
            // (`L4W59DynamicInvokerExact`).
            || (kind == MH_KIND_DYNAMIC_INVOKER && crate::nbflags().mh_dynamic_invoker_site_type))
    {
        return None;
    }
    // A lambda proxy's handle needs no arm of its own (interpreter round i4
    // wave 93, lane L4): wave 62 asked the JDK's `MethodType` here because the
    // render spelled the proxy `Object`; `unnameable_mirror_descriptor` now
    // spells it `L<host>$$Lambda/0x<id>;`, so `(P)int` differs from an
    // `(Object)int` site below and `method_type_display_in` names it `P`
    // (`L4W62LambdaProxyOwnerCast` `exact-*` rows, `L4W92UnnameableMirror`
    // `exact-erased`, `--compatible`). With
    // `CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME=0` the proxy renders as `Object`
    // again and an erased site is accepted, as before wave 62.
    // The handle's `mh_declared_descriptor`, read once by the door (round 12
    // wave 7).
    let declared = declared?;
    if declared == call_site {
        // Equal STRINGS are not equal types when a name has two classes
        // (JIT round 17 wave 2, lane mh2; [`mh_exact_loader_refusal`]).
        return mh_exact_loader_refusal(ctx, mh, call_site);
    }
    // `finish_method_handle` writes a FABRICATED `()V` whenever no usable
    // descriptor was supplied ("C21: Always populate `type`"), so that
    // JDK-internal walks never read a null MethodType. That is a stand-in for
    // "unknown", not a claim that the handle takes nothing and returns nothing
    // -- and treating it as one would turn every such handle into a throw at
    // its first real call site. Unknown is an accept, here as everywhere else
    // in this check.
    if declared == "()V" && call_site != "()V" {
        return None;
    }
    // Both must be well-formed before a difference between them means anything.
    let shown_declared = method_type_display_in(&*ctx, &declared)?;
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    // JIT round 17 wave 8, lane mh7: the handle's type's `toString()` runs
    // first ([`mh_wmte_message_icce`]).
    if let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") {
        let ids = mh_type_class_ids(&*ctx, mt);
        if let Some(icce) = mh_wmte_message_icce(ctx, &ids) {
            return Some(icce);
        }
    }
    // JDK 25's wording (`Invokers.newWrongMethodTypeException`), measured by
    // `tools/probes/interp/L4/L4W23InvokeExactMessage.java`; `expected ...
    // but found ...` was an older JDK's (wave 23, lane L4).
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::ExactType,
        &mh_exact_type_text(&shown_declared, &shown_site),
    ))
}

/// `CRATONVM_MH_REAL_UNSPELLABLE_TYPE_ADAPTS` (default on; wave 62, lane L4):
/// see [`mh_real_invoke_adapt`]'s unspellable-type arm.
pub(crate) fn mh_real_unspellable_type_adapts() -> bool {
    crate::nbflags().mh_real_unspellable_type_adapts
}

/// `invoke` on a REAL `MethodHandle` (pure lever) is `asType(callSiteType)`
/// then an exact call: when the handle's own `type()` is not the call site's,
/// answer the adapted handle (the JDK's own `asType`, so a varargs collector
/// collects and a boxing adapter boxes). `Ok(None)` when no adaptation applies.
pub(crate) fn mh_real_invoke_adapt(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    call_site: Option<&str>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // SBR3 lane mh: ungated. Only a REAL handle (no carrier `MH_KIND` slot)
    // reaches the body below; before this a retired `Lookup.find*` row left it
    // answering `null`.
    let Some(call_site) = call_site else {
        return Ok(None);
    };
    if !mh_is_real_handle(&*ctx, this) {
        return Ok(None);
    }
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return Ok(None);
    };
    match crate::link_to_method_descriptor(&*ctx, mt) {
        Some(declared) if declared != call_site => {}
        // A type this model cannot spell (a lambda proxy's mirror names no
        // class) is no proof the site matches: the JDK's `asType` decides,
        // and answers the handle itself when the types are equal. Skipping it
        // ran `findVirtual(P, "run", ..).invoke((Object) "x")` on the
        // `String` (`NoSuchMethodError`) and a proxy field getter read a slot
        // of the `String`, where HotSpot casts (interpreter round i2 wave 62,
        // lane L4; `L4W62LambdaProxyOwnerCast`, `--jdk-only`).
        None if mh_real_unspellable_type_adapts() => {}
        _ => return Ok(None),
    }
    // JIT round 17 wave 4 (lane mheditor): a VM-minted carrier with no
    // `MH_KIND` slot (a `Linker.downcallHandle` carrier, an instance of the
    // ABSTRACT `MethodHandle` itself) is not a real handle: the JDK's `asType`
    // reaches `rebind().editor()` and fails with `NoSuchMethodError:
    // MethodHandle.editor()`. Its `invoke` stays with the door below
    // (`mh_downcall_door_type`), as before the SBR3 ungating.
    if mh_is_vm_carrier_without_kind(&*ctx, this) && mh_real_adapt_skips_carrier_enabled() {
        return Ok(None);
    }
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let site_mt = mh_site_method_type(&mut scope, call_site)?;
    let Some(site_mt) = site_mt else {
        return Ok(None);
    };
    let this_cur = scope.get(&this_h);
    // Wave 114 (`CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ`): an `asTypeCache`
    // hit is answered by reading it (`mh_real_as_type_bytecode`).
    let adapted = match site_mt {
        Value::Object(Some(mt)) => mh_real_as_type_bytecode(&mut *scope, this_cur, mt)?,
        other => scope.invoke_virtual_bytecode_only(
            this_cur,
            "asType",
            "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
            &[other],
        )?,
    };
    Ok(match adapted {
        Some(Value::Object(Some(h))) => Some(h),
        _ => None,
    })
}

/// The `MethodType` of a signature-polymorphic call site's descriptor,
/// resolved as the site's link resolves it: by the JDK factory
/// (`fromMethodDescriptorString`, which interns) with the CALLING class's
/// loader. `null` there means the system loader, which cannot see a class of
/// a user loader's own jar: JBoss Logging's `constructor.invoke(logger)`
/// inside a Spring Boot fat jar (site `(Lorg/jboss/logging/Logger;)...`,
/// `Logger` defined by `LaunchedClassLoader`) failed with
/// `TypeNotPresentException` and Hibernate Validator's
/// `ConfigurationImpl.<clinit>` with it, where HotSpot starts the jar.
/// Interpreter round i2 wave 64, lane L5 follow-up
/// (`docs/feature-designs/class-resolution-and-caching.md`).
pub(crate) fn mh_site_method_type(
    scope: &mut NativeHandleScope<'_>,
    call_site: &str,
) -> Result<Option<Value>, MethodCallFailed> {
    let caller = scope.frame_class_ids_tail(1).0.first().copied();
    // Interpreter round i3 wave 72, lane L4 (`CRATONVM_MH_SITE_TYPE_TABLE`):
    // the site's type, resolved once per (descriptor, built-in loader) as
    // HotSpot resolves an `invokehandle` site's `MethodType` once at link
    // time. A hit allocates nothing and runs no Java.
    let key_loader = if mh_site_type_table_enabled() {
        caller.and_then(|c| scope.loader_id_of_class_definite(c))
    } else {
        None
    };
    let key = key_loader.and_then(|l| mh_site_type_key(Some(l), call_site));
    if let Some(key) = key.as_deref() {
        if let Some(mt) = scope.bound_method_type_get(key) {
            return Ok(Some(Value::Object(Some(mt))));
        }
    }
    // A caller whose `getClassLoader()` answered `null` AND whose defining
    // loader is the bootstrap one (id 0) is a boot-loader class. The by-name
    // call alone is not proof: under `--compatible` it answered `null` for an
    // application class while the cached dispatch path yielded the native to
    // the JDK body (dev 2026-10-11, fixed by `57f3176f87`), and the flipped
    // switch then failed `L4W79BootSiteType` `control`. Such a `null` keeps
    // the system loader's meaning.
    let mut boot_caller = false;
    let loader = match caller {
        Some(caller) => {
            let mirror = scope.get_class_mirror(caller);
            match scope.invoke_virtual(mirror, "getClassLoader", "()Ljava/lang/ClassLoader;", &[]) {
                Ok(Some(Value::Object(Some(loader)))) => Some(scope.root(loader)),
                Ok(Some(Value::Object(None))) => {
                    boot_caller = scope.loader_id_of_class_definite(caller) == Some(0);
                    None
                }
                _ => None,
            }
        }
        None => None,
    };
    let text = scope.create_string_uninterned(call_site);
    let loader = loader.map(|h| scope.get(&h));
    // Interpreter round i3 wave 79, lane L4 (`CRATONVM_MH_SITE_TYPE_BOOT_LOADER`,
    // default off): `fromMethodDescriptorString`'s `null` loader is the SYSTEM
    // loader, so a boot-loader caller's site resolved its reference components
    // through the application loader; HotSpot links the site with the caller's
    // own (boot) loader. `MethodType.fromDescriptor(desc, null)` (package-private,
    // the JDK's own boot meaning of `null`) does that; a library without it (the
    // synthetic JDK) keeps the public factory.
    //
    // The boot loader is used only when the caller frame is exact
    // (`innermost_frame_class_exact`): a compiled JDK method that inlined a
    // user method's `invoke` site reports its own (boot) class, and the system
    // loader -- which sees every class the boot loader sees -- keeps such a
    // site resolving as before.
    let factory = if boot_caller
        && mh_site_type_boot_loader_enabled()
        && scope.innermost_frame_class_exact()
        && scope.method_exists(
            "java/lang/invoke/MethodType",
            "fromDescriptor",
            MH_SITE_TYPE_FACTORY_DESC,
        ) {
        "fromDescriptor"
    } else {
        "fromMethodDescriptorString"
    };
    let answer = match scope.invoke_static(
        "java/lang/invoke/MethodType",
        factory,
        MH_SITE_TYPE_FACTORY_DESC,
        &[Value::Object(Some(text)), Value::Object(loader)],
    ) {
        Ok(answer) => answer,
        // Interpreter round i3 wave 81, lane L4: a site type naming a class the
        // loader cannot find fails the way HotSpot's link fails it.
        Err(failed) => return Err(mh_site_type_link_error(&mut **scope, failed, boot_caller)),
    };
    // No GC point between the factory's return and the store: `mt` is current.
    if let (Some(key), Some(Value::Object(Some(mt)))) = (key.as_deref(), answer) {
        let max_loader = key_loader.unwrap_or(0);
        if mh_site_type_row_admissible(&**scope, call_site, mt, max_loader) {
            let row = scope.bound_method_type_put(key, mt);
            return Ok(Some(Value::Object(Some(row))));
        }
    }
    Ok(answer)
}

/// The failure of a site-type factory call, as HotSpot's `invokehandle` link
/// raises it (interpreter round i3 wave 81, lane L4; page
/// `i72-L4-a-bootstrap-callers-converting-invoke-resolves-its-site-through-the-system-loader`).
///
/// HotSpot resolves each reference component of the site's descriptor with
/// the caller's loader in `NCDFError` mode
/// (`SystemDictionary::find_method_handle_type` -> `SignatureStream::as_java_mirror`
/// -> `resolve_or_fail` -> `handle_resolution_exception`): a class the loader
/// cannot find is `NoClassDefFoundError(<internal name>)`, whose cause is the
/// loader's `ClassNotFoundException` for a Java loader and none for the boot
/// loader (its lookup raises nothing). The JDK factory this VM resolves the
/// site with (`BytecodeDescriptor.parseMethod`) raises
/// `TypeNotPresentException(<binary name>, cnfe)` instead, which no HotSpot
/// `invoke` site ever throws. Any other failure passes through unchanged, as
/// does a `TypeNotPresentException` whose name cannot be read.
///
/// Unconditional: it changes only the type of an exception already being
/// thrown, on the error path (cold); success and failure are decided as before.
#[cold]
#[inline(never)]
pub(crate) fn mh_site_type_link_error(
    ctx: &mut dyn NativeContext,
    failed: MethodCallFailed,
    boot_caller: bool,
) -> MethodCallFailed {
    let MethodCallFailed::ExceptionThrown(tnpe) = &failed else {
        return failed;
    };
    let tnpe = *tnpe;
    let is_tnpe = ctx
        .class_name_of_id(ctx.class_id_of_object(tnpe))
        .is_some_and(|n| n == "java/lang/TypeNotPresentException");
    if !is_tnpe {
        return failed;
    }
    let type_name = match ctx.get_field_by_name(tnpe, "typeName") {
        Value::Object(Some(name)) => ctx.read_string(name),
        _ => None,
    };
    let Some(type_name) = type_name.filter(|n| !n.is_empty()) else {
        return failed;
    };
    // `Throwable.cause == this` is "not set".
    let cause = if boot_caller {
        None
    } else {
        match ctx.get_field_by_name(tnpe, "cause") {
            Value::Object(Some(cause)) if cause != tnpe => Some(cause),
            _ => None,
        }
    };
    // Both allocations below may move the original throwable and its cause.
    let tnpe_pin = ctx.pin_native_root(tnpe);
    let cause_pin = cause.map(|c| ctx.pin_native_root(c));
    let message = ctx.create_string_uninterned(&type_name.replace('.', "/"));
    // The VM's `construct_throwable` pins its argument values itself.
    let built = ctx.construct_throwable(
        "java/lang/NoClassDefFoundError",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    );
    let tnpe = ctx.read_native_pin(tnpe_pin, tnpe);
    let cause = match (cause_pin, cause) {
        (Some(pin), Some(c)) => Some(ctx.read_native_pin(pin, c)),
        _ => None,
    };
    ctx.unpin_native_roots(tnpe_pin);
    match built {
        Ok(Some(ncdfe)) => {
            if let Some(cause) = cause {
                ctx.set_field_by_name(ncdfe, "cause", Value::Object(Some(cause)));
            }
            MethodCallFailed::ExceptionThrown(ncdfe)
        }
        Err(thrown) => thrown,
        Ok(None) => MethodCallFailed::ExceptionThrown(tnpe),
    }
}

/// The descriptor of both site-type factories, `fromMethodDescriptorString`
/// and `fromDescriptor` (`(String, ClassLoader)MethodType`).
pub(crate) const MH_SITE_TYPE_FACTORY_DESC: &str =
    "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;";

/// `CRATONVM_MH_SITE_TYPE_BOOT_LOADER` (default ON since 2026-10-11, `0` turns
/// it off; correctness; interpreter round i3 wave 79, lane L4; page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// see [`mh_site_method_type`]. The caller is the frame `frame_class_ids_tail`
/// reports, and a boot-loader frame standing between user code and the call
/// would move a site naming an application class from "resolves" to
/// `NoClassDefFoundError` (the Spring Boot fat-jar failure of wave 64 was the
/// system-loader version of that). Wave 81 found such a frame: a compiled frame
/// whose inline chain is not exact pushes only its owner's class, not the
/// inlined callee's (`vm/src/runtime/stackwalker.rs` `push_compiled_class_ids`).
/// So the boot loader is used only when `innermost_frame_class_exact` says the
/// reported class is the running code's; otherwise the system loader (which
/// also sees every boot class) keeps the site resolving as before.
pub(crate) fn mh_site_type_boot_loader_enabled() -> bool {
    crate::nbflags().mh_site_type_boot_loader
}

/// `CRATONVM_MH_SITE_TYPE_TABLE` -- default OFF (opt-in, performance;
/// interpreter round i3 wave 72, lane L4, proposal
/// `i72-L4-proposal-jdk-only-field-handle-mint-and-converting-invoke-cost`).
/// `invoke` on a REAL `MethodHandle` at a call site of another type
/// ([`mh_real_invoke_adapt`]) and the unspellable-type `invokeExact` refusal
/// build the site's `MethodType` through the JDK's own
/// `fromMethodDescriptorString` (a `Class.getClassLoader` call, a descriptor
/// parse and a `Class.forName` through the caller's loader per reference
/// component, then the intern table) on EVERY call. HotSpot resolves it once,
/// when the `invokehandle` site links. Under the switch the answer is kept in
/// this VM's bound-`MethodType` table, keyed by the descriptor and the
/// caller's BUILT-IN defining loader, and a later call of the same shape takes
/// it with no Java run. The stored object is the JDK's interned instance,
/// which the row keeps strongly reachable, so it is the very object a fresh
/// `fromMethodDescriptorString` would answer: nothing Java can observe moves.
pub(crate) fn mh_site_type_table_enabled() -> bool {
    crate::nbflags().mh_site_type_table
}

/// Marks a site-type row of the bound-`MethodType` table (SOH): a method
/// descriptor never holds one, and a row key holding no NUL names no class
/// ids, so `MethodTypeTable::forget_classes` never drops a site row (it names
/// only built-in loaders' classes, which never unload).
pub(crate) const MH_SITE_TYPE_KEY_TAG: char = '\u{1}';

/// The table key of the call site `desc` resolved for a caller whose defining
/// loader is `loader` (`loader_id_of_class_definite`): only a KNOWN built-in
/// loader (0 bootstrap, 1 platform, 2 application), whose classes never unload
/// (a row is rooted outright, like every row of the table); `None` for any
/// other loader, an unknown one, or a text that is not a method descriptor.
pub(crate) fn mh_site_type_key(loader: Option<i32>, desc: &str) -> Option<String> {
    let loader = loader.filter(|l| (0..=2).contains(l))?;
    if !desc.starts_with('(') || desc.contains(MH_SITE_TYPE_KEY_TAG) || desc.contains('\0') {
        return None;
    }
    Some(format!(
        "{MH_SITE_TYPE_KEY_TAG}{loader}{MH_SITE_TYPE_KEY_TAG}{desc}"
    ))
}

/// May the site type `mt`, just answered for `desc`, become a row? Every
/// component must be a primitive (or `void`), an array of a primitive, or a
/// class whose defining loader, read off its own `ClassId`, is KNOWN to be a
/// built-in one (`ClassLoader.getSystemClassLoader()` may be a user loader
/// under `-Djava.system.class.loader`, and a row naming its class would keep
/// that loader alive for the VM's life). An array of a class is refused
/// outright (its loader is its element's, which this does not read). Reads
/// only; asked on a miss.
///
/// A row under loader `max_loader`'s key may name only classes that loader
/// sees by delegation: a loader id no greater than its own (0 bootstrap, 1
/// platform, 2 application). A boot-loader caller whose site was resolved
/// through the system loader (no `fromDescriptor`, or an inexact caller
/// frame) and found an application class is not filed under loader 0, where
/// a later boot-loader caller would take a type HotSpot cannot link.
pub(crate) fn mh_site_type_row_admissible(
    ctx: &dyn NativeContext,
    desc: &str,
    mt: ObjectRef,
    max_loader: i32,
) -> bool {
    let Value::Object(Some(rtype)) = ctx.get_field_by_name(mt, "rtype") else {
        return false;
    };
    let Value::Object(Some(ptypes)) = ctx.get_field_by_name(mt, "ptypes") else {
        return false;
    };
    let mut mirrors = Vec::with_capacity(ctx.array_length(ptypes));
    for i in 0..ctx.array_length(ptypes) {
        match ctx.get_array_element(ptypes, i) {
            Value::Object(Some(m)) => mirrors.push(m),
            _ => return false,
        }
    }
    mh_site_type_components_builtin(ctx, desc, &mirrors, rtype, max_loader)
}

/// [`mh_site_type_row_admissible`] over the type's mirrors: `ptypes` in
/// order, then `rtype`, judged against `desc`'s tokens.
pub(crate) fn mh_site_type_components_builtin(
    ctx: &dyn NativeContext,
    desc: &str,
    ptypes: &[ObjectRef],
    rtype: ObjectRef,
    max_loader: i32,
) -> bool {
    let Some((params, ret)) = split_descriptor_params(desc) else {
        return false;
    };
    if params.len() != ptypes.len() {
        return false;
    }
    params
        .iter()
        .zip(ptypes.iter())
        .chain(std::iter::once((&ret, &rtype)))
        .all(|(token, mirror)| mh_site_type_component_builtin(ctx, token, *mirror, max_loader))
}

/// One component of [`mh_site_type_components_builtin`].
pub(crate) fn mh_site_type_component_builtin(
    ctx: &dyn NativeContext,
    token: &str,
    mirror: ObjectRef,
    max_loader: i32,
) -> bool {
    match token.as_bytes().first() {
        Some(b'L') => ctx.class_id_from_mirror(mirror).is_some_and(|id| {
            ctx.loader_id_of_class_definite(id)
                .is_some_and(|l| (0..=max_loader.min(2)).contains(&l))
        }),
        Some(b'[') => {
            let elem = token.trim_start_matches('[');
            elem.len() == 1 && b"ZBCSIJFD".contains(&elem.as_bytes()[0])
        }
        Some(c) => token.len() == 1 && b"ZBCSIJFDV".contains(c),
        None => false,
    }
}

/// Is `mh` a VM-minted FFM downcall carrier? With a real JDK image the carrier
/// is allocated as `DirectMethodHandle`, not the bare abstract `MethodHandle`;
/// either way it has no usable `form`. Use the tagged carrier predicate so
/// both image shapes stay on the VM's downcall doors. Reads only.
/// JIT round 17 wave 4, lane mheditor
/// (`r17-mh-downcall-invoke-editor-nosuchmethoderror-FIXED-20261005.md`).
pub(crate) fn mh_is_vm_carrier_without_kind(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    crate::panama::is_downcall_handle(ctx, mh)
}

/// `CRATONVM_MH_REAL_ADAPT_SKIPS_CARRIER` -- correctness, default ON, `=0`
/// lets [`mh_real_invoke_adapt`] run the JDK's `asType` on a VM-minted carrier
/// again (the `NoSuchMethodError: MethodHandle.editor()` of
/// `downcall.invoke(Integer.valueOf(-4))`). JIT round 17 wave 4, lane
/// mheditor. Read only for such a carrier at a converting call site.
pub(crate) fn mh_real_adapt_skips_carrier_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_REAL_ADAPT_SKIPS_CARRIER")
}

/// Does `invokeWithArguments` hand `this` to the JDK's own `asType(generic)`
/// and spun entry ([`mh_real_invoke_with_arguments_door`])? A real handle, but
/// not a VM-minted carrier ([`mh_is_vm_carrier_without_kind`]): the JDK body
/// on a `Linker.downcallHandle` carrier reached `makePairwiseConvertByEditor`
/// and died in `NoSuchMethodError: MethodHandle.editor()` in both modes
/// (`R16FfmcostPlainMemo` `invokeWithArguments`), while the model's door below
/// treats the carrier as a static handle of its type
/// (`mh_downcall_door_type`). `CRATONVM_MH_REAL_ADAPT_SKIPS_CARRIER=0` restores
/// the old routing (`FFM-door-seg-probe-residuals-FIXED-20261007.md`).
pub(crate) fn mh_with_arguments_runs_real(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    // The switch is read only for a downcall carrier (an uncached env read).
    mh_is_real_jdk_handle(ctx, this)
        || (mh_is_real_handle(ctx, this) && !mh_real_adapt_skips_carrier_enabled())
}

/// Is `this` the JDK's own variable-arity wrapper
/// (`MethodHandleImpl$AsVarargsCollector`), the one class that overrides
/// `invokeWithArguments(Object...)` (JDK 25)? Reads only. Interpreter round i4
/// wave 84, lane L4.
pub(crate) fn mh_is_real_varargs_collector(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(this))
        .is_some_and(|n| &*n == "java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
}

/// `MethodHandle.asTypeUncached(newType)` — a reviewed `Intrinsic` over the
/// JDK's own body, for one receiver only: a VM-minted `Linker.downcallHandle`
/// carrier (`panama::alloc_downcall_handle`, tagged on either the abstract
/// `MethodHandle` or concrete `DirectMethodHandle` shape, with no usable
/// `form`). The JDK body on it runs
/// `makePairwiseConvertByEditor`, whose `target.rebind().editor()` has no
/// method to call there (`NoSuchMethodError: MethodHandle.editor()`): under
/// `--jdk-only` every `asType` of a downcall handle (the registered `asType`
/// `Bridge` yields to the real `asType` bytecode, which calls this), and in
/// both modes the `asType` inside the JDK's `invokeWithArguments`. The carrier
/// answers the adapted copy the `asType` `Bridge` mints
/// ([`mh_downcall_as_type`], HotSpot's refusals included). Every other
/// receiver runs the JDK's own body unchanged (`AsVarargsCollector`, which
/// overrides this method, never reaches here). `None`: not a carrier, or
/// `CRATONVM_MH_REAL_ADAPT_SKIPS_CARRIER=0`.
pub(crate) fn mh_as_type_uncached_carrier(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: Value,
) -> Option<MethodCallResult> {
    let Value::Object(Some(mt)) = new_type else {
        return None;
    };
    if !mh_real_adapt_skips_carrier_enabled()
        || !mh_downcall_adapt_enabled()
        || !crate::panama::is_downcall_handle(&*ctx, this)
    {
        return None;
    }
    Some(mh_downcall_as_type(ctx, this, mt))
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 82, lane L4: `asTypeUncached` answers a converting
// adapter minted from a filed one (`CRATONVM_MH_REAL_AS_TYPE_MEMO`, default
// off; proposal `i72-L4-proposal-jdk-only-field-handle-mint-and-converting-invoke-cost`,
// item 4)
// ---------------------------------------------------------------------------
//
// `getter.bindTo(box).invoke()` under `--jdk-only` adapts every FRESH bound
// handle through the JDK's `asType` -> `asTypeUncached` ->
// `makePairwiseConvertByEditor` (`computeValueConversions`, a `HashMap`,
// `rebind`, the form editor, `copyWithExtendL` through the species factory):
// ~28 us per call without a JIT, where the exact path costs ~3.8 us. HotSpot
// does the same work per fresh handle; only its cost differs.
//
// For a receiver that is a `DirectMethodHandle` (any species) or a
// `BoundMethodHandle$Species_*`, JDK 25's answer is a function of the
// receiver's class, `type` and `form` and of the new type, EXCEPT where the
// receiver itself goes in: `rebind()` of a direct handle is
// `makeReinvoker(this)` (the receiver is the new species' `argL0`), and of a
// bound one is `this` (its bound values are copied, same names, by
// `copyWithExtendL`) unless `tooComplex()` (field count and form size: in the
// key) makes it a reinvoker too. The rest -- the conversion functions
// (`ValueConversions` constants), the edited form (the editor's transform
// cache) -- is shared.
//
// So, per thread, VM and collection epoch, keyed by (receiver class, `type`,
// `form`, new type):
// * FIRST answer: filed as a CANDIDATE when it is a `BoundMethodHandle$Species_*`
//   whose `type` is the new type and whose every bound field is either the
//   receiver itself, the receiver's same-named bound value, or something else
//   (a "constant", read back from this filed answer) -- and the receiver went
//   in whole: exactly once as itself (a reinvoker), or (bound receiver) all of
//   its bound values by name.
// * SECOND answer for the key, on a DIFFERENT receiver: promoted when it is
//   exactly what the candidate predicts for that receiver (same class, same
//   `form`, the receiver / its values where filed, every constant the very
//   same object or value); refused for the epoch otherwise. A fresh object
//   per call (`MH_cast.bindTo(Class)` for a reference cast) therefore never
//   promotes, and neither does anything that holds the receiver indirectly.
// * SECOND answer on the candidate's OWN receiver (interpreter round i4 wave
//   84, lane L4: one handle adapted to two types in turn, which misses the
//   JDK's one-entry `asTypeCache` every call): the same check; when it holds,
//   the key mints for that receiver only (`PromotedSelf` -- a constant that
//   equals one of its bound values is not told apart from a copy, so another
//   receiver still has to answer once, and then promotes or refuses the key).
// * A PROMOTED key mints: a fresh instance of the filed species with the new
//   type, the filed form, the receiver / its bound values and the constants.
//   Its `asTypeCache`, `asTypeSoftCache`, `customizationCount` and
//   `updateInProgress` keep a fresh handle's defaults, as the body's answer's.
//
// The receiver's own cache is NOT this code's business: `asType`'s
// `setAsTypeCache` called `asTypeUncached` and writes `asTypeCache` or a
// `SoftReference` (`isSafeToCache`) itself, exactly as on HotSpot. A
// `WrongMethodTypeException`, any throw, a collection during the body, and
// `CRATONVM_DBG_MH_DISPATCH` decline (nothing filed). `AsVarargsCollector`,
// `DelegatingMethodHandle` and the other overriders of `asTypeUncached` never
// reach this native, and any other receiver class is not admitted.

/// `CRATONVM_MH_REAL_AS_TYPE_MEMO` -- performance, default OFF, `truthy_word`.
pub(crate) fn mh_real_as_type_memo_enabled() -> bool {
    crate::nbflags().mh_real_as_type_memo
}

/// Lines of a thread's `asType` memo (a power of two, direct-mapped).
pub(crate) const AS_TYPE_MEMO_SLOTS: usize = 64;
/// `MethodHandle.asTypeUncached`'s descriptor.
pub(crate) const AS_TYPE_UNCACHED_DESC: &str =
    "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
/// The bound-handle species' class-name prefix (JDK 25: `Species_L` is
/// declared in `BoundMethodHandle`, the others are spun with this name).
pub(crate) const AS_TYPE_SPECIES_PREFIX: &str = "java/lang/invoke/BoundMethodHandle$Species_";

/// The admitted receiver shapes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AsTypeReceiver {
    /// A `DirectMethodHandle` of any species: `rebind()` is a reinvoker.
    Direct,
    /// A bound species: `rebind()` is itself (or a reinvoker when too complex).
    Bound,
}

/// The receiver shape of a class named `name`, or `None` (not admitted).
pub(crate) fn as_type_memo_receiver_kind(name: &str) -> Option<AsTypeReceiver> {
    if name.starts_with(AS_TYPE_SPECIES_PREFIX) {
        Some(AsTypeReceiver::Bound)
    } else if name == "java/lang/invoke/DirectMethodHandle"
        || name.starts_with("java/lang/invoke/DirectMethodHandle$")
    {
        Some(AsTypeReceiver::Direct)
    } else {
        None
    }
}

/// Where a filed answer's bound field comes from at a mint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AsTypeSource {
    /// The receiver itself (a reinvoker's `argL0`).
    Receiver,
    /// The receiver's bound field of the same name.
    ReceiverField,
    /// The filed answer's own value (a shared conversion function).
    Constant,
}

/// One bound field of the filed species.
#[derive(Clone, Debug)]
pub(crate) struct AsTypeField {
    pub(crate) name: Box<str>,
    pub(crate) source: AsTypeSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AsTypeState {
    Candidate,
    Promoted,
    /// Interpreter round i4 wave 84 (lane L4): the second answer came from
    /// the candidate's OWN receiver and was exactly the candidate's
    /// prediction, so every constant is proven constant (a fresh-per-call
    /// value such as `MH_cast.bindTo(Class)` refuses), but a constant that
    /// happens to equal one of that receiver's bound values cannot be told
    /// apart from a copy of it. Minted for that receiver only; another
    /// receiver runs the body, whose answer promotes or refuses the key as
    /// a [`AsTypeState::Candidate`]'s would.
    PromotedSelf,
    Refused,
}

/// The state the second (or a later, re-checked) answer for a key files:
/// `same_receiver` is "the candidate's own receiver answered again",
/// `predicted` is [`as_type_memo_predicts`]'s verdict on that answer. `None`
/// keeps the line as it is. Pure.
pub(crate) fn as_type_memo_next_state(
    state: AsTypeState,
    same_receiver: bool,
    predicted: bool,
) -> Option<AsTypeState> {
    match state {
        AsTypeState::Candidate | AsTypeState::PromotedSelf => Some(if !predicted {
            AsTypeState::Refused
        } else if same_receiver {
            AsTypeState::PromotedSelf
        } else {
            AsTypeState::Promoted
        }),
        AsTypeState::Promoted | AsTypeState::Refused => None,
    }
}

/// A memo key: the receiver's class and its `type` / `form` objects, and the
/// new type, by address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AsTypeMemoKey {
    pub(crate) recv_class: u32,
    pub(crate) recv_type: usize,
    pub(crate) recv_form: usize,
    pub(crate) new_type: usize,
}

/// One filed answer, valid for VM `vm` in collection epoch `epoch` only.
#[derive(Clone)]
pub(crate) struct AsTypeMemoLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) key: AsTypeMemoKey,
    pub(crate) state: AsTypeState,
    /// The candidate's receiver (a promotion needs a different one).
    pub(crate) first_receiver: usize,
    pub(crate) answer: ObjectRef,
    pub(crate) answer_class: ClassId,
    pub(crate) form: Value,
    pub(crate) fields: std::rc::Rc<[AsTypeField]>,
}

thread_local! {
    /// Per-thread `asType` adapter memo (see the section comment).
    pub(crate) static AS_TYPE_MEMO: std::cell::RefCell<Vec<Option<AsTypeMemoLine>>> =
        std::cell::RefCell::new(vec![None; AS_TYPE_MEMO_SLOTS]);
}

/// The memo line of `key` (a mixed hash, as [`finder_mint_slot`]).
pub(crate) fn as_type_memo_slot(key: &AsTypeMemoKey) -> usize {
    let mut h = u64::from(key.recv_class)
        ^ (key.recv_type as u64).rotate_left(17)
        ^ (key.recv_form as u64).rotate_left(31)
        ^ (key.new_type as u64).rotate_left(47);
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((h ^ (h >> 31)) as usize) & (AS_TYPE_MEMO_SLOTS - 1)
}

/// Two field values the same: references by identity, primitives by bits.
pub(crate) fn as_type_memo_same_value(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            x.map(|o| o.as_ptr() as usize) == y.map(|o| o.as_ptr() as usize)
        }
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        _ => false,
    }
}

/// Is `value` the object `o`?
pub(crate) fn as_type_memo_is(value: Value, o: ObjectRef) -> bool {
    matches!(value, Value::Object(Some(v)) if v.as_ptr() == o.as_ptr())
}

/// The instance fields `class` itself declares, or `None` when a class between
/// it and `java/lang/invoke/MethodHandle` declares any (a layout this memo does
/// not know). Reads only.
pub(crate) fn as_type_memo_own_fields(
    ctx: &dyn NativeContext,
    class: ClassId,
) -> Option<Vec<cratonvm_native_api::registry::FieldMetadata>> {
    let own: Vec<_> = ctx
        .declared_fields(class)
        .into_iter()
        .filter(|f| !f.is_static)
        .collect();
    let mut cur = ctx.superclass_of(class);
    for _ in 0..8 {
        let c = cur?;
        if ctx.class_name_arc_of_id(c).as_deref() == Some("java/lang/invoke/MethodHandle") {
            return Some(own);
        }
        if ctx.declared_fields(c).iter().any(|f| !f.is_static) {
            return None;
        }
        cur = ctx.superclass_of(c);
    }
    None
}

/// The candidate line for `answer`, the body's first answer to `key` on
/// receiver `this`, or `None` when it is not admissible (see the section
/// comment). Reads only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn as_type_memo_candidate(
    ctx: &dyn NativeContext,
    vm: usize,
    epoch: u64,
    key: AsTypeMemoKey,
    kind: AsTypeReceiver,
    this: ObjectRef,
    new_type: ObjectRef,
    answer: ObjectRef,
) -> Option<AsTypeMemoLine> {
    let answer_class = ctx.class_id_of_object(answer);
    if !ctx
        .class_name_arc_of_id(answer_class)?
        .starts_with(AS_TYPE_SPECIES_PREFIX)
    {
        return None;
    }
    if !as_type_memo_is(ctx.get_field_by_name(answer, "type"), new_type) {
        return None;
    }
    let form = ctx.get_field_by_name(answer, "form");
    if !matches!(form, Value::Object(Some(_))) {
        return None;
    }
    let answer_fields = as_type_memo_own_fields(ctx, answer_class)?;
    let recv_fields = match kind {
        AsTypeReceiver::Bound => as_type_memo_own_fields(ctx, ctx.class_id_of_object(this))?,
        AsTypeReceiver::Direct => Vec::new(),
    };
    let mut fields: Vec<AsTypeField> = Vec::with_capacity(answer_fields.len());
    let (mut as_receiver, mut as_values) = (0usize, 0usize);
    for f in &answer_fields {
        let value = ctx.get_field_by_name(answer, &f.name);
        let source = if as_type_memo_is(value, this) {
            as_receiver += 1;
            AsTypeSource::Receiver
        } else if recv_fields.iter().any(|r| {
            r.name == f.name
                && r.descriptor == f.descriptor
                && as_type_memo_same_value(value, ctx.get_field_by_name(this, &r.name))
        }) {
            as_values += 1;
            AsTypeSource::ReceiverField
        } else {
            AsTypeSource::Constant
        };
        fields.push(AsTypeField {
            name: f.name.clone().into_boxed_str(),
            source,
        });
    }
    // The receiver went in whole: once as itself (a reinvoker), or -- a bound
    // receiver -- every bound value by name.
    let whole = (as_receiver == 1 && as_values == 0)
        || (kind == AsTypeReceiver::Bound
            && as_receiver == 0
            && !recv_fields.is_empty()
            && as_values == recv_fields.len());
    if !whole {
        return None;
    }
    Some(AsTypeMemoLine {
        vm,
        epoch,
        key,
        state: AsTypeState::Candidate,
        first_receiver: this.as_ptr() as usize,
        answer,
        answer_class,
        form,
        fields: fields.into(),
    })
}

/// Is `answer` (the body's answer on receiver `this`) exactly what `line`
/// predicts for `this`? Reads only.
pub(crate) fn as_type_memo_predicts(
    ctx: &dyn NativeContext,
    line: &AsTypeMemoLine,
    this: ObjectRef,
    new_type: ObjectRef,
    answer: ObjectRef,
) -> bool {
    if ctx.class_id_of_object(answer) != line.answer_class
        || !as_type_memo_is(ctx.get_field_by_name(answer, "type"), new_type)
        || !as_type_memo_same_value(ctx.get_field_by_name(answer, "form"), line.form)
    {
        return false;
    }
    line.fields.iter().all(|f| {
        let got = ctx.get_field_by_name(answer, &f.name);
        let want = match f.source {
            AsTypeSource::Receiver => Value::Object(Some(this)),
            AsTypeSource::ReceiverField => ctx.get_field_by_name(this, &f.name),
            AsTypeSource::Constant => ctx.get_field_by_name(line.answer, &f.name),
        };
        as_type_memo_same_value(got, want)
    })
}

/// A fresh adapter of `line`'s species for receiver `this`. `None` when the
/// allocation fails (the caller then runs the body, which raises it).
pub(crate) fn as_type_memo_mint(
    ctx: &mut dyn NativeContext,
    line: &AsTypeMemoLine,
    this: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallResult> {
    let Value::Object(Some(form)) = line.form else {
        return None;
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let new_type_h = scope.root(new_type);
    let proto_h = scope.root(line.answer);
    let form_h = scope.root(form);
    let fresh = match scope.instantiate(
        line.answer_class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        _ => return None,
    };
    let fresh_h = scope.root(fresh);
    let this = scope.get(&this_h);
    let proto = scope.get(&proto_h);
    let fresh = scope.get(&fresh_h);
    let new_type = scope.get(&new_type_h);
    let form = scope.get(&form_h);
    scope.set_field_by_name(fresh, "type", Value::Object(Some(new_type)));
    scope.set_field_by_name(fresh, "form", Value::Object(Some(form)));
    for f in line.fields.iter() {
        let value = match f.source {
            AsTypeSource::Receiver => Value::Object(Some(this)),
            AsTypeSource::ReceiverField => scope.get_field_by_name(this, &f.name),
            AsTypeSource::Constant => scope.get_field_by_name(proto, &f.name),
        };
        scope.set_field_by_name(fresh, &f.name, value);
    }
    Some(Ok(Some(Value::Object(Some(fresh)))))
}

/// `asTypeUncached` under `CRATONVM_MH_REAL_AS_TYPE_MEMO`: a minted adapter
/// for a promoted key, else the JDK's own body (its answer filed or checked).
/// `None` when the memo does not apply (switch off, not `--jdk-only`, a null
/// type, a receiver class not admitted): the caller runs the body.
pub(crate) fn mh_real_as_type_memo(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: Value,
) -> Option<MethodCallResult> {
    if !mh_real_as_type_memo_enabled() || !ctx.is_jdk_only() || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let Value::Object(Some(nt)) = new_type else {
        return None;
    };
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let epoch = ctx.gc_address_epoch();
    let recv_class = ctx.class_id_of_object(this);
    let recv_name = ctx.class_name_arc_of_id(recv_class)?;
    let kind = as_type_memo_receiver_kind(&recv_name)?;
    let Value::Object(Some(recv_type)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    let Value::Object(Some(recv_form)) = ctx.get_field_by_name(this, "form") else {
        return None;
    };
    let key = AsTypeMemoKey {
        recv_class: recv_class.as_u32(),
        recv_type: recv_type.as_ptr() as usize,
        recv_form: recv_form.as_ptr() as usize,
        new_type: nt.as_ptr() as usize,
    };
    let slot = as_type_memo_slot(&key);
    let line = AS_TYPE_MEMO
        .with(|m| m.borrow()[slot].clone())
        .filter(|l| l.vm == vm && l.epoch == epoch && l.key == key);
    match line.as_ref().map(|l| (l.state, l.first_receiver)) {
        Some((AsTypeState::Promoted, _)) => {
            if let Some(line) = &line {
                if let Some(answer) = as_type_memo_mint(ctx, line, this, nt) {
                    return Some(answer);
                }
            }
        }
        // Wave 84: a key proven on its own receiver mints for that receiver.
        Some((AsTypeState::PromotedSelf, first)) if first == this.as_ptr() as usize => {
            if let Some(line) = &line {
                if let Some(answer) = as_type_memo_mint(ctx, line, this, nt) {
                    return Some(answer);
                }
            }
        }
        Some((AsTypeState::Refused, _)) => return None,
        Some((AsTypeState::Candidate | AsTypeState::PromotedSelf, _)) | None => {}
    }
    let answer = ctx.invoke_virtual_bytecode_only(
        this,
        "asTypeUncached",
        AS_TYPE_UNCACHED_DESC,
        &[new_type],
    );
    // `this`, `nt` and the key were read before the body (a GC point).
    if ctx.gc_address_epoch() != epoch {
        return Some(answer);
    }
    // `adapted` is a copied reference: `answer` stays whole.
    let adapted = match answer {
        Ok(Some(Value::Object(Some(adapted)))) => adapted,
        _ => return Some(answer),
    };
    let filed = match line {
        Some(line)
            if matches!(
                line.state,
                AsTypeState::Candidate | AsTypeState::PromotedSelf
            ) =>
        {
            // Wave 84: the candidate's own receiver answering again is
            // evidence too (`AsTypeState::PromotedSelf`); it used to keep the
            // line a candidate, so one handle adapted to two types in turn
            // (the JDK's one-entry `asTypeCache` misses every call) never
            // minted.
            let same_receiver = line.first_receiver == this.as_ptr() as usize;
            let predicted = as_type_memo_predicts(&*ctx, &line, this, nt, adapted);
            as_type_memo_next_state(line.state, same_receiver, predicted)
                .map(|state| AsTypeMemoLine { state, ..line })
        }
        _ => as_type_memo_candidate(&*ctx, vm, epoch, key, kind, this, nt, adapted),
    };
    if let Some(filed) = filed {
        AS_TYPE_MEMO.with(|m| m.borrow_mut()[slot] = Some(filed));
    }
    Some(answer)
}

/// `invokeExact` on a REAL `MethodHandle` (pure lever): its own `type()` must
/// equal the call site's, else JDK 25's `WrongMethodTypeException: handle's
/// method type (..)R but found (..)R` (`Invokers.checkExactType`).
pub(crate) fn mh_real_exact_refusal(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    call_site: Option<&str>,
) -> Option<MethodCallFailed> {
    let call_site = call_site?;
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let record = real_handle_record(ctx, this);
    let owned;
    let declared: &str = match &record {
        Some(record) => &record.declared,
        None => {
            let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
                return None;
            };
            owned = match crate::link_to_method_descriptor(&*ctx, mt) {
                Some(owned) => owned,
                // A type this model cannot spell (wave 62, lane L4): the JDK
                // compares, as `checkExactType` would.
                None if mh_real_unspellable_type_adapts() => {
                    return mh_real_unspellable_exact_refusal(ctx, this, call_site);
                }
                None => return None,
            };
            &owned
        }
    };
    if declared == call_site {
        // JIT round 17 wave 2, lane mh2: the loader half of the comparison.
        return mh_exact_loader_refusal(ctx, this, call_site);
    }
    let shown_declared = method_type_display_in(&*ctx, declared)?;
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    // JIT round 17 wave 8, lane mh7: the handle's type's `toString()` runs
    // first ([`mh_wmte_message_icce`]).
    // The record path has no `MethodType` in hand: read the handle's own.
    let ids = match ctx.get_field_by_name(this, "type") {
        Value::Object(Some(mt)) => mh_type_class_ids(&*ctx, mt),
        _ => Vec::new(),
    };
    if let Some(icce) = mh_wmte_message_icce(ctx, &ids) {
        return Some(icce);
    }
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::ExactType,
        &mh_exact_type_text(&shown_declared, &shown_site),
    ))
}

/// `CRATONVM_MH_EXACT_LOADER_CHECK` -- correctness, default ON, `=0` restores
/// the string-only `invokeExact` comparison (JIT round 17 wave 2, lane mh2;
/// [`mh_exact_loader_refusal`]). Read only for a site that names a class
/// outside `java/` ([`mh_site_names_ambiguous_class`]).
pub(crate) fn mh_exact_loader_check_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_EXACT_LOADER_CHECK")
}

/// Does `desc` name a class, outside `java/` (which only the boot and
/// platform loaders define), for which `f` answers `true`? The names are the
/// reference components' (array elements unwrapped). No allocation.
pub(crate) fn mh_descriptor_any_loader_name(desc: &str, mut f: impl FnMut(&str) -> bool) -> bool {
    let mut rest = desc;
    while let Some(at) = rest.find('L') {
        let tail = &rest[at + 1..];
        let Some(end) = tail.find(';') else {
            return false;
        };
        let name = &tail[..end];
        if !name.starts_with("java/") && f(name) {
            return true;
        }
        rest = &tail[end + 1..];
    }
    false
}

/// Does the call-site descriptor `site` name a class that two or more
/// loaders define? Only then can equal descriptor STRINGS be unequal types.
/// `false` with [`mh_exact_loader_check_enabled`] off, which is read only for
/// a site naming a class outside `java/` (a primitive site pays nothing).
pub(crate) fn mh_site_names_ambiguous_class(ctx: &dyn NativeContext, site: &str) -> bool {
    let mut enabled: Option<bool> = None;
    mh_descriptor_any_loader_name(site, |name| {
        *enabled.get_or_insert_with(mh_exact_loader_check_enabled)
            && matches!(
                ctx.classify_class_name(name),
                cratonvm_native_api::NameLookup::Ambiguous { .. }
            )
    })
}

/// The `WrongMethodTypeException` an `invokeExact` whose site descriptor
/// STRING equals the handle's type must still raise because the two name
/// different classes (JIT round 17 wave 2, lane mh2;
/// `r17-mh-constant-bind-admission-is-loader-blind-FIXED-20261005.md`).
///
/// HotSpot resolves the site's `MethodType` with the CALLER's loader (JVMS
/// §5.4.3.6) and `Invokers.checkExactType` compares it with the handle's by
/// identity: a handle of `(LBox;)I` minted against loader A's `Box`, invoked
/// from a class whose `Box` is loader B's, is `handle's method type (Box)int
/// but found (Box)int`. The door compared the strings and called the member.
///
/// Asked only when `site` names a class with two or more definitions
/// ([`mh_site_names_ambiguous_class`]): the caller is the innermost frame (the
/// door pushes none), each reference mirror of the handle's `type` whose name
/// is such a class is compared with what that name resolves to from the
/// caller (`class_id_by_name_near`), and only a resolved, DIFFERENT class
/// refuses. A JDK caller (`java/`, `jdk/`, `sun/`: the generic doors'
/// own `invokeExact`) and every unknown are accepts, the door's rule.
pub(crate) fn mh_exact_loader_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    site: &str,
) -> Option<MethodCallFailed> {
    if !mh_site_names_ambiguous_class(&*ctx, site) {
        return None;
    }
    let caller = *ctx.frame_class_ids_tail(1).0.first()?;
    let caller_name = ctx.class_name_of_id(caller)?;
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| caller_name.starts_with(p))
    {
        return None;
    }
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return None;
    };
    let mut mirrors: Vec<ObjectRef> = Vec::new();
    if let Value::Object(Some(r)) = ctx.get_field_by_name(mt, "rtype") {
        mirrors.push(r);
    }
    if let Value::Object(Some(p)) = ctx.get_field_by_name(mt, "ptypes") {
        for i in 0..ctx.array_length(p) {
            if let Value::Object(Some(m)) = ctx.get_array_element(p, i) {
                mirrors.push(m);
            }
        }
    }
    let differs = mirrors
        .iter()
        .any(|&m| mh_mirror_seen_differently(&*ctx, m, caller).is_some());
    if !differs {
        return None;
    }
    // JIT round 17 wave 8, lane mh7: HotSpot's message is
    // `"handle's method type " + actual + " but found " + expected`, and each
    // `MethodType.toString()` asks `getSimpleName()` of its return type, then
    // its parameter types.
    let mut ids: Vec<ClassId> = mirrors
        .iter()
        .filter_map(|&m| mirror_class_id(&*ctx, m))
        .collect();
    let seen: Vec<ClassId> = ids
        .iter()
        .filter_map(|&id| {
            let name = ctx.class_name_of_id(mh_element_class_id(&*ctx, id))?;
            mh_caller_sees(&*ctx, &name, caller)
        })
        .collect();
    ids.extend(seen);
    if let Some(icce) = mh_wmte_message_icce(ctx, &ids) {
        return Some(icce);
    }
    let shown = method_type_display_in(&*ctx, site)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::ExactType,
        &mh_exact_type_text(&shown, &shown),
    ))
}

/// `id`'s bottom element class (`id` itself for a non-array). 255 is the JVMS
/// dimension limit.
pub(crate) fn mh_element_class_id(ctx: &dyn NativeContext, id: ClassId) -> ClassId {
    let mut id = id;
    for _ in 0..256 {
        match ctx.array_component_class_id(id) {
            Some(component) => id = component,
            None => break,
        }
    }
    id
}

/// The class ids of the method type `mt`'s return type, then its parameter
/// types, in the order `MethodType.toString()` names them.
pub(crate) fn mh_type_class_ids(ctx: &dyn NativeContext, mt: ObjectRef) -> Vec<ClassId> {
    let mut ids = Vec::new();
    if let Value::Object(Some(r)) = ctx.get_field_by_name(mt, "rtype") {
        ids.extend(mirror_class_id(ctx, r));
    }
    if let Value::Object(Some(p)) = ctx.get_field_by_name(mt, "ptypes") {
        for i in 0..ctx.array_length(p) {
            if let Value::Object(Some(m)) = ctx.get_array_element(p, i) {
                ids.extend(mirror_class_id(ctx, m));
            }
        }
    }
    ids
}

/// The `IncompatibleClassChangeError` HotSpot raises while BUILDING a door's
/// `WrongMethodTypeException` message, or `None` (JIT round 17 wave 8, lane
/// mh7; `R17Mh6Door chain-cross-exact-pub`, `R17Mh7Inner`).
/// `Invokers.newWrongMethodTypeException` concatenates the two
/// `MethodType`s, whose `toString()` calls `Class.getSimpleName()` on each
/// class (`classes`, in that order); for a nested class whose outer class
/// resolves the nested name to another class (a child-first loader that
/// defines `Outer$Inner` but not `Outer`) that is `getDeclaringClass0`'s
/// `IncompatibleClassChangeError: Outer and Outer$Inner disagree on
/// InnerClasses attribute`, and it replaces the WMTE. Gated by
/// `CRATONVM_CLASS_INNER_CLASSES_CHECK` and free unless a user-defined loader
/// has defined a class.
pub(crate) fn mh_wmte_message_icce(
    ctx: &mut dyn NativeContext,
    classes: &[ClassId],
) -> Option<MethodCallFailed> {
    for &id in classes {
        if let Some(message) = crate::lang_class::simple_name_inner_classes_disagreement(ctx, id) {
            return Some(crate::phases_early::throw_jca_exc(
                ctx,
                "java/lang/IncompatibleClassChangeError",
                &message,
            ));
        }
    }
    None
}

/// The class of the handle's type mirror `m` -- an array mirror's element
/// class (JIT round 17 wave 2, lane mh3; item 3 of
/// `r17-mh-door-loader-check-residuals-FIXED-20261005.md`: an array's identity is its
/// element's, JVMS §5.3.3) -- when the class `caller` resolves its name, without
/// loading, to ANOTHER class of that name; `None` otherwise. Only names outside
/// `java/` that two or more loaders define are asked; every unknown is `None`
/// (the doors' accept). Before round 17 wave 2 lane mh3 an array mirror was
/// skipped, so `(Box[])int` across loaders compared as a string.
pub(crate) fn mh_mirror_seen_differently(
    ctx: &dyn NativeContext,
    m: ObjectRef,
    caller: ClassId,
) -> Option<ClassId> {
    let mut id = mirror_class_id(ctx, m)?;
    let mut name = ctx.class_name_of_id(id)?;
    // A depth bound for a malformed chain: 255 is the JVMS dimension limit.
    let mut depth = 0;
    while name.starts_with('[') {
        depth += 1;
        if depth > 255 {
            return None;
        }
        id = ctx.array_component_class_id(id)?;
        name = ctx.class_name_of_id(id)?;
    }
    if name.starts_with("java/")
        || !matches!(
            ctx.classify_class_name(&name),
            cratonvm_native_api::NameLookup::Ambiguous { .. }
        )
    {
        return None;
    }
    mh_caller_sees(ctx, &name, caller)
        .filter(|seen| *seen != id)
        .map(|_| id)
}

/// `CRATONVM_MH_DOOR_INITIATED_VIEW` -- correctness, default ON, `=0` turns it
/// off (JIT round 17 wave 6, lane mh6; the door half of item 2 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): see
/// [`mh_caller_sees`]. Read only for a name two loaders define. Landed ON; the
/// orchestrator's `R17Mh6Door` run (w6b, 2026-10-05) printed byte-identical
/// output with and without it, because every line of that probe answered
/// first with an `IllegalAccessError` (a package-private box) or the
/// `InnerClasses` ICCE (a nested box), so it was moved to opt-in. JIT round 18
/// wave 11 (lane mhresid) turned it back ON with the lines that reach the
/// comparison: a PUBLIC TOP-LEVEL class a caller's loader delegates to a
/// user-defined parent (`R18MhresidLinks chain-cross-exact-top`,
/// `chain2-cross-exact-top-null`). Both modes: the initiating records it
/// reads are kept under `--compatible` too (`HotSpotAlignment::InitiatingRecords`,
/// on by default there).
pub(crate) fn mh_door_initiated_view_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_DOOR_INITIATED_VIEW")
}

/// `CRATONVM_MH_SITE_ACCESS_CHECK` -- correctness, default ON, `=0` restores
/// round 17 wave 6's first answer (JIT round 17 wave 6, lane mh6, second
/// pass): see [`mh_site_access_refusal`]. Read only for a site naming a class
/// two loaders define.
pub(crate) fn mh_site_access_check_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_SITE_ACCESS_CHECK")
}

/// The `IllegalAccessError` HotSpot raises when an `invokeExact` / `invoke`
/// call site's descriptor names a class its caller may not access (JIT round
/// 17 wave 6, lane mh6; `R17Mh6Door chain-cross-return`,
/// `chain2-cross-exact-null`).
///
/// HotSpot links the site by resolving its `MethodType` through the caller
/// (`SystemDictionary::find_method_handle_type`), which, for every class the
/// descriptor names, emulates `ConstantPool::verify_constant_pool_resolve`:
/// `LinkResolver::check_klass_accessibility(caller, klass)`. A package-private
/// class of another loader is another runtime package (JVMS §5.3), so the link
/// fails with `failed to access class Box from class Caller (Box is in unnamed
/// module of loader L @h; Caller is in unnamed module of loader M @h)` before
/// the handle's type is compared or the member runs, on every execution (a
/// failed link is not cached). The doors compared types first and raised
/// `WrongMethodTypeException` / `ClassCastException`. The interpreted link
/// throws the same error (`invoke.rs` `mh_site_link_checked`); this is the
/// door's copy, for compiled and reflective callers.
///
/// Asked only when the site names a class two loaders define (the only shape
/// where the doors' own checks could answer first, and a classification the
/// doors already pay for); a JDK caller and every unknown are accepts. Names
/// in descriptor order, the first refusal wins. The message is the VM's
/// constant-pool door's (`class_access_denial_for_name`).
pub(crate) fn mh_site_access_refusal(ctx: &dyn NativeContext, site: Option<&str>) -> Option<MethodCallFailed> {
    let site = site?;
    let mut enabled: Option<bool> = None;
    let ambiguous = mh_descriptor_any_loader_name(site, |name| {
        *enabled.get_or_insert_with(mh_site_access_check_enabled)
            && matches!(
                ctx.classify_class_name(name),
                cratonvm_native_api::NameLookup::Ambiguous { .. }
            )
    });
    if !ambiguous {
        return None;
    }
    let caller = *ctx.frame_class_ids_tail(1).0.first()?;
    let caller_name = ctx.class_name_of_id(caller)?;
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| caller_name.starts_with(p))
    {
        return None;
    }
    let mut message: Option<String> = None;
    mh_descriptor_any_loader_name(site, |name| {
        message = ctx.class_access_denial_for_name(caller, name);
        message.is_some()
    });
    Some(MethodCallFailed::InternalError(VmError::Linkage(
        LinkageError::IllegalAccessError { message: message? },
    )))
}

/// The class `name` denotes from `caller`, without loading: what the MH
/// doors compare a handle's type mirror with. The VM's constant-pool view
/// first (`class_id_seen_by_class_without_loading`: the caller's loader's own
/// definitions, its direct supertypes and the names it INITIATED, which is
/// how a name delegated to a user-defined PARENT loader is seen -- the site's
/// link, `jit_bridge.rs` `mh_site_link_caller_types`, drove that delegation),
/// then `class_id_by_name_near`, the doors' reader before round 17 wave 6
/// (lane mh6), which walks only the caller's own namespace and the built-in
/// chain and so answered `None` -- the doors' accept -- for a delegated name.
pub(crate) fn mh_caller_sees(ctx: &dyn NativeContext, name: &str, caller: ClassId) -> Option<ClassId> {
    if mh_door_initiated_view_enabled() {
        if let Some(seen) = ctx.class_id_seen_by_class_without_loading(name, caller) {
            return Some(seen);
        }
    }
    ctx.class_id_by_name_near(name, caller)
}

/// `CRATONVM_MH_INVOKE_LOADER_CAST` -- correctness, default ON, `=0` restores
/// round 17 wave 2's mh2 behaviour (JIT round 17 wave 2, lane mh3; item 2 of
/// `r17-mh-door-loader-check-residuals-FIXED-20261005.md`): see
/// [`mh_invoke_loader_cast_refusal`]. Read only for a site naming a class two
/// loaders define.
pub(crate) fn mh_invoke_loader_cast_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_LOADER_CAST")
}

/// The `ClassCastException` `mh.invoke(...)` must raise when the call site's
/// descriptor STRING equals the handle's type but a parameter names, from the
/// caller, another loader's class of that name, and the argument is not an
/// instance of the handle's class (JIT round 17 wave 2, lane mh3).
///
/// `invoke` is `asType(siteType).invokeExact(...)`, with `siteType` resolved
/// through the caller's loader. Two same-named classes of two loaders are
/// unequal but CONVERTIBLE: `MethodType.canConvert` accepts any reference to
/// reference conversion ("R->R always works, since null is always valid
/// dynamically"), so `asType` builds a cast adapter, and the cast
/// (`MethodHandleImpl.castReference`) raises `Cannot cast <name> to <name>` for
/// a non-null argument of the caller's class, while a `null` argument reaches
/// the member. (Not the `WrongMethodTypeException` the residuals page guessed:
/// that would need an unconvertible pair.) Every door short-cuts equal strings
/// as exact and called the member with the other loader's object. A return
/// type seen differently is cast on the way out instead
/// ([`mh_invoke_return_cast_target`], JIT round 17 wave 3, lane mh4).
pub(crate) fn mh_invoke_loader_cast_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    args: &[Value],
    site: Option<&str>,
) -> Option<MethodCallFailed> {
    let site = site?;
    if !mh_site_names_ambiguous_class(&*ctx, site) || !mh_invoke_loader_cast_enabled() {
        return None;
    }
    let caller = *ctx.frame_class_ids_tail(1).0.first()?;
    let caller_name = ctx.class_name_of_id(caller)?;
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| caller_name.starts_with(p))
    {
        return None;
    }
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return None;
    };
    let declared = crate::link_to_method_descriptor(&*ctx, mt)?;
    if declared != site {
        // Unequal strings take the converting path, whose JDK `asType` casts.
        return None;
    }
    let Value::Object(Some(ptypes)) = ctx.get_field_by_name(mt, "ptypes") else {
        return None;
    };
    let count = ctx.array_length(ptypes);
    if count != args.len() {
        return None;
    }
    for (i, arg) in args.iter().enumerate() {
        let Value::Object(Some(value)) = arg else {
            continue;
        };
        let Value::Object(Some(m)) = ctx.get_array_element(ptypes, i) else {
            continue;
        };
        // Arrays were left to the member until JIT round 17 wave 3 (lane mh4;
        // item 3 of `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`):
        // the cast is to the handle's own array class (the mirror itself), and
        // an array argument is an instance of it when its element classes
        // are, dimension by dimension ([`mh_array_instance_of`]); under
        // `CRATONVM_MH_INVOKE_LOADER_CAST_FULL`.
        let Some(target) = mirror_class_id(&*ctx, m) else {
            continue;
        };
        let target_is_array = match ctx.class_name_of_id(target) {
            Some(n) => n.starts_with('['),
            None => continue,
        };
        if target_is_array && !mh_invoke_loader_cast_full_enabled() {
            continue;
        }
        if mh_mirror_seen_differently(&*ctx, m, caller).is_none() {
            continue;
        }
        // A reference array's `class_id_of_object` is its COMPONENT class
        // (`entry_value_type_display`), so the array argument is judged by
        // the bytecode's own identity verdict, else component against the
        // target's component, and named by its own array name (JIT round 17
        // wave 5, lane mh5: lane mh4 compared the component with the array
        // class, which refused a fitting array too, and printed `Cannot cast
        // R17Mh4Box to [LR17Mh4Box;` where HotSpot prints `Cannot cast
        // [LR17Mh4Box; to [LR17Mh4Box;`).
        let from = ctx.class_id_of_object(*value);
        let fits = if target_is_array {
            ctx.object_is_array(*value)
                && match ctx.array_instance_of_array_class(*value, target) {
                    Some(verdict) => verdict,
                    None => match ctx.array_component_class_id(target) {
                        Some(component) => mh_array_instance_of(&*ctx, from, component),
                        None => true,
                    },
                }
        } else {
            from == target || ctx.is_subclass(from, target)
        };
        if fits {
            continue;
        }
        let from_name = entry_value_type_display(&*ctx, *value);
        let to_name = ctx.class_name_of_id(target)?.class_text(&*ctx);
        return Some(
            RuntimeError::ClassCastException {
                message: format!("Cannot cast {from_name} to {to_name}"),
            }
            .into(),
        );
    }
    None
}

/// `CRATONVM_MH_INVOKE_LOADER_CAST_FULL` -- correctness, default ON, `=0`
/// restores round 17 wave 2's casts (JIT round 17 wave 3, lane mh4; item 3 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): `invoke`'s `asType`
/// casts across loaders also cover an array argument
/// ([`mh_invoke_loader_cast_refusal`]) and the member's reference result
/// ([`mh_invoke_return_cast_target`]). Read only for a site naming a class two
/// loaders define.
pub(crate) fn mh_invoke_loader_cast_full_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_LOADER_CAST_FULL")
}

/// Is an object of class `from` an instance of the ARRAY class `to` (JVMS
/// §6.5 `checkcast`, arrays): `from` an array of as many dimensions or more,
/// each element class an instance of the other's, down to the leaves; an
/// array deeper than `to` fits an `Object` / `Cloneable` / `Serializable`
/// leaf. An unknown name or component answers `true` (the doors' accept). JIT
/// round 17 wave 3, lane mh4.
pub(crate) fn mh_array_instance_of(ctx: &dyn NativeContext, from: ClassId, to: ClassId) -> bool {
    let (mut f, mut t) = (from, to);
    // 255 is the JVMS dimension limit.
    for _ in 0..=255 {
        if f == t {
            return true;
        }
        let (Some(f_name), Some(t_name)) = (ctx.class_name_of_id(f), ctx.class_name_of_id(t))
        else {
            return true;
        };
        match (f_name.starts_with('['), t_name.starts_with('[')) {
            (true, true) => {
                let (Some(fc), Some(tc)) = (
                    ctx.array_component_class_id(f),
                    ctx.array_component_class_id(t),
                ) else {
                    return true;
                };
                f = fc;
                t = tc;
            }
            (false, true) => return false,
            (true, false) => {
                return matches!(
                    &*t_name,
                    "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
                )
            }
            (false, false) => return ctx.is_subclass(f, t),
        }
    }
    true
}

/// The class `mh.invoke(...)`'s result must be cast to on the way out, when
/// the call site's descriptor STRING equals the handle's type but its return
/// type names, from the caller, ANOTHER loader's class of that name (JIT round
/// 17 wave 3, lane mh4; item 3 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`).
///
/// `invoke` is `asType(siteType).invokeExact(...)`: the adapter casts the
/// member's result to the site's return class (`MethodHandleImpl.castReference`,
/// `Cannot cast <name> to <name>` after the member ran, for a non-null result
/// of the handle's class; a `null` passes). `Some((caller's class, caller's
/// class name))` only for a plain class return seen differently.
///
/// JIT round 18 wave 9, lane mhloader: an ARRAY return is cast too, to the
/// caller's array class (the element class the caller sees, at the handle's
/// dimension, filed under that element's defining loader, JVMS 5.3.3), as
/// HotSpot's adapter does: `Cannot cast [LBox; to [LBox;`. The array class is
/// found or created (no Java runs); every unknown keeps the member's answer.
/// Asked before the call.
pub(crate) fn mh_invoke_return_cast_target(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    site: Option<&str>,
) -> Option<ClassId> {
    let site = site?;
    let ret = site.rsplit_once(')')?.1;
    if !(ret.starts_with('L') || ret.starts_with('['))
        || !mh_site_names_ambiguous_class(&*ctx, ret)
        || !mh_invoke_loader_cast_enabled()
        || !mh_invoke_loader_cast_full_enabled()
    {
        return None;
    }
    let caller = *ctx.frame_class_ids_tail(1).0.first()?;
    let caller_name = ctx.class_name_of_id(caller)?;
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| caller_name.starts_with(p))
    {
        return None;
    }
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return None;
    };
    if crate::link_to_method_descriptor(&*ctx, mt)? != site {
        return None;
    }
    let Value::Object(Some(r)) = ctx.get_field_by_name(mt, "rtype") else {
        return None;
    };
    let ret_name = ctx.class_name_of_id(mirror_class_id(&*ctx, r)?)?;
    // The seen-differently test unwraps an array to its element.
    let handle_class = mh_mirror_seen_differently(&*ctx, r, caller)?;
    let name = ctx.class_name_of_id(handle_class)?;
    let seen = mh_caller_sees(&*ctx, &name, caller).filter(|seen| *seen != handle_class)?;
    if !ret_name.starts_with('[') {
        return Some(seen);
    }
    let dims = ret_name.bytes().take_while(|b| *b == b'[').count();
    let array_name = format!("{}L{name};", "[".repeat(dims));
    let loader = ctx.loader_id_of_class(seen);
    if loader < 0 {
        return None;
    }
    let target = ctx.array_class_id_for_loader(&array_name, loader as u32)?;
    // The array class must be over the element the caller sees.
    (mh_element_class_id(&*ctx, target) == seen).then_some(target)
}

/// Apply [`mh_invoke_return_cast_target`]'s cast to `invoke`'s `result` (JIT
/// round 17 wave 3, lane mh4): a non-null object that is not an instance of
/// `target` is HotSpot's `ClassCastException`. Pure reads.
pub(crate) fn mh_invoke_return_cast(
    ctx: &dyn NativeContext,
    target: ClassId,
    result: MethodCallResult,
) -> MethodCallResult {
    let Ok(Some(Value::Object(Some(value)))) = &result else {
        return result;
    };
    // An array target (round 18 wave 9, lane mhloader): judged like the
    // argument cast in `mh_invoke_loader_cast_refusal` (a reference array's
    // `class_id_of_object` is its component class), named by the array's own
    // name.
    if ctx
        .class_name_of_id(target)
        .is_some_and(|n| n.starts_with('['))
    {
        let from = ctx.class_id_of_object(*value);
        let fits = ctx.object_is_array(*value)
            && match ctx.array_instance_of_array_class(*value, target) {
                Some(verdict) => verdict,
                None => match ctx.array_component_class_id(target) {
                    Some(component) => mh_array_instance_of(ctx, from, component),
                    None => true,
                },
            };
        if fits {
            return result;
        }
        let Some(to_name) = ctx.class_name_of_id(target) else {
            return result;
        };
        let from_name = entry_value_type_display(ctx, *value);
        return Err(RuntimeError::ClassCastException {
            message: format!("Cannot cast {from_name} to {}", to_name.class_text(&*ctx)),
        }
        .into());
    }
    let from = ctx.class_id_of_object(*value);
    // t1 lane H: a reference array's `class_id_of_object` is its COMPONENT's,
    // so a `String[]` passed for a `CharSequence` target. An array is an
    // instance of `Object`, `Cloneable` and `Serializable` only (JLS 10.8).
    if ctx.object_is_array(*value) {
        let fits = ctx.class_name_of_id(target).is_none_or(|n| {
            matches!(
                n.as_str(),
                "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
            )
        });
        if fits {
            return result;
        }
        let Some(to_name) = ctx.class_name_of_id(target) else {
            return result;
        };
        let from_name = entry_value_type_display(ctx, *value);
        return Err(RuntimeError::ClassCastException {
            message: format!("Cannot cast {from_name} to {}", to_name.class_text(&*ctx)),
        }
        .into());
    }
    if from == target || ctx.is_subclass(from, target) {
        return result;
    }
    let (Some(from_name), Some(to_name)) =
        (ctx.class_name_of_id(from), ctx.class_name_of_id(target))
    else {
        return result;
    };
    Err(RuntimeError::ClassCastException {
        message: format!(
            "Cannot cast {} to {}",
            from_name.class_text(&*ctx),
            to_name.class_text(&*ctx)
        ),
    }
    .into())
}

/// [`mh_real_exact_refusal`] for a real handle whose `type()` this model
/// cannot spell (a lambda proxy's mirror names no class): the JDK's
/// `MethodType.equals` against the call site's type, and on a mismatch
/// `checkExactType`'s text from the two types' own `toString()`. A failure to
/// build or ask either type is no refusal (the old answer). Interpreter round
/// i2 wave 62, lane L4: `findGetter(P, "arg$1", int).invokeExact((Object) p)`
/// ran where HotSpot raises `WrongMethodTypeException` (`--jdk-only`).
pub(crate) fn mh_real_unspellable_exact_refusal(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    call_site: &str,
) -> Option<MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let site = match mh_site_method_type(&mut scope, call_site) {
        Ok(Some(Value::Object(Some(site)))) => site,
        _ => return None,
    };
    let site_h = scope.root(site);
    let this_cur = scope.get(&this_h);
    let Value::Object(Some(own)) = scope.get_field_by_name(this_cur, "type") else {
        return None;
    };
    let own_h = scope.root(own);
    let site = scope.get(&site_h);
    match scope.invoke_virtual(
        own,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(site))],
    ) {
        Ok(Some(Value::Int(0))) => {}
        _ => return None,
    }
    let mut shown = Vec::with_capacity(2);
    for h in [&own_h, &site_h] {
        let mt = scope.get(h);
        match scope.invoke_virtual(mt, "toString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => shown.push(scope.read_string(s)?),
            _ => return None,
        }
    }
    let (shown_declared, shown_site) = (&shown[0], &shown[1]);
    Some(mh_door_wmte(
        &mut *scope,
        MhWmteRaise::ExactType,
        &mh_exact_type_text(shown_declared, shown_site),
    ))
}

/// The primitive descriptor char of a single descriptor token, or `None` for a
/// reference or array type.
pub(crate) fn primitive_char(token: &str) -> Option<u8> {
    let b = token.as_bytes();
    match (b.len(), b.first()) {
        (1, Some(c @ (b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D' | b'Z'))) => Some(*c),
        _ => None,
    }
}

/// JLS 5.1.2 widening primitive conversion: can `from` reach `to` without a
/// cast? `boolean` reaches nothing and nothing reaches it.
pub(crate) fn primitive_widens(from: u8, to: u8) -> bool {
    if from == to {
        return true;
    }
    let reach: &[u8] = match from {
        b'B' => b"SIJFD",
        b'S' | b'C' => b"IJFD",
        b'I' => b"JFD",
        b'J' => b"FD",
        b'F' => b"D",
        _ => b"",
    };
    reach.contains(&to)
}

/// The `WrongMethodTypeException` `mh.invoke(...)` must raise when a call-site
/// PRIMITIVE argument would have to NARROW to reach the handle's declared
/// parameter, or `None`.
///
/// `invoke` is `asType(callSiteType)` followed by an exact invocation, and
/// `asType` performs the method-invocation conversions -- which include
/// widening but NOT narrowing. `(int) findStatic(Math,"max",(int,int)int)
/// .invoke(1L, 2L)` is a `WrongMethodTypeException` on HotSpot; here it
/// answered `0`, so the missing refusal came with a wrong VALUE attached.
///
/// **Primitive positions only, and that restriction is the safety argument.**
/// The reference half of this conversion is a `cast`, i.e. an assignability
/// question, and `bindTo`'s in-tree note records why that is not yet safe: the
/// only predicate `NativeContext` offers is `is_subclass`, which answers FALSE
/// for a fabricated stand-in against a real JDK interface, and `bindTo` sits on
/// the Groovy-indy / SpEL / log4j paths where interfaces and subtypes are the
/// norm. A false `ClassCastException` there refuses working code. A primitive
/// pair needs no hierarchy at all: both sides are single descriptor chars and
/// the conversion table is closed, so this cannot be wrong for a reason outside
/// its own two lines.
///
/// Arity is deliberately not this rule's business -- a length mismatch means
/// the two descriptors are not describing the same call and every positional
/// comparison below would be meaningless, so it returns `None` and leaves
/// whatever handles arity to handle it.
/// May `value` be passed where the parameter descriptor `target_desc` is
/// declared -- as far as this VM is willing to ASSERT?
///
/// `None` means "cannot tell, do not refuse". `Some(false)` is a POSITIVE
/// mismatch, and the only thing a caller may turn into a `ClassCastException`.
///
/// # Why every clause below is a refusal to answer
///
/// `MethodHandle.invoke` and `bindTo` both end in a `cast`, so both need this,
/// and both sit on the Groovy-indy / SpEL-FunctionReference / log4j-provider
/// path in this tree. A FALSE `ClassCastException` there refuses working code,
/// which is strictly worse than the wrong answer it would replace -- so the
/// predicate is built to fail towards "allow":
///
/// * a null is always passable, and a primitive is not this predicate's question;
/// * `Ljava/lang/Object;` accepts anything; an ARRAY parameter takes
///   `checkcast`'s array rule ([`reference_array_target_admitted`], wave 60;
///   it accepted every value before);
/// * an UNRESOLVABLE target class means we know nothing;
/// * a value that is itself a VM-minted stand-in is never judged, and
///   `synthetic_implements_declared` is consulted for the relationships that
///   live in the interpreter's table rather than in the loaded hierarchy;
/// * a `java.lang.reflect.Proxy` instance and a lambda proxy acquire their
///   interfaces at RUNTIME, invisibly to any static walk, so neither is judged.
///   These are the two hatches `typecheck::aastore_element_assignable` carries
///   for the same reason, and they are the whole cost of judging interfaces at
///   all.
///
/// An INTERFACE target IS judged, but only through
/// `class_assignable_to_name` -- the loader-blind by-name walk over supers and
/// interfaces that the `checkcast`/`aastore` path already uses.
/// `is_subclass` alone must never decide an interface: it compares `ClassId`s,
/// and under a forked loader it refuses a value whose chain names the target
/// under a different id.
///
/// W7-19 5.1 deferred the `bindTo` half of this for exactly the reasons above.
/// What changed is not the risk but the predicates available to price it -- and
/// that `probes/CodegenFrameworkSmoke.java` now boots Groovy, ByteBuddy,
/// Mockito, ASM, Javassist and Objenesis as themselves, so the refusal has a
/// lane that can catch it being wrong.
pub(crate) fn reference_arg_admitted(
    ctx: &mut dyn NativeContext,
    value: Value,
    target_desc: &str,
) -> Option<bool> {
    let Value::Object(Some(obj)) = value else {
        return None;
    };
    if target_desc.starts_with('[') {
        // An ARRAY target is `checkcast`'s array rule (wave 60, lane L4):
        // the heap shape, then the VM's own component verdict.
        return reference_array_target_admitted(&*ctx, obj, target_desc);
    }
    if !target_desc.starts_with('L') || !target_desc.ends_with(';') {
        return None;
    }
    let target = &target_desc[1..target_desc.len() - 1];
    if target == "java/lang/Object" {
        return None;
    }
    // An ARRAY is an instance of `Object`, `Cloneable` and `Serializable` and of
    // no other class or interface (JLS 10.8), whether or not the target is
    // loaded. Settled here from the heap's object kind because the class id
    // below cannot settle it: an array's header carries its COMPONENT's class
    // id, so an `H[]` read as an `H` and was admitted as an `H` receiver
    // (r11w13-orch-varhandle-set-on-an-array-receiver-throws-aioobe-not-cce,
    // and `Integer[]` into an `Integer` field).
    if ctx.object_is_array(obj) {
        return Some(matches!(
            target,
            "java/lang/Cloneable" | "java/io/Serializable"
        ));
    }
    let value_cid = ctx.class_id_of_object(obj);
    // The common case first (round 12 wave 7, lane mh5): a value of exactly
    // the named class. One read-locked `Arc<str>` clone instead of the
    // unique-name lookup, a `String` of the value's name and the hierarchy
    // walks below -- `invoke` asks this for every reference argument, twice
    // (declared and leaf). An answer every caller already treats as an allow:
    // each refuses only on `Some(false)`, and the slow path answers this case
    // `Some(true)` or, for a name several loaders define, `None`.
    if ctx.class_name_arc_of_id(value_cid).as_deref() == Some(target) {
        return Some(true);
    }
    // A lambda proxy target, by its own name (`unnameable_mirror_descriptor`,
    // interpreter round i4 wave 92, lane L4): a final hidden class, so only
    // its own instances (HotSpot's `Class.cast`). It used to be unresolvable
    // here (`None`, admitted), and before that it rendered as `Object`.
    if let Some(proxy) = lambda_proxy_descriptor_class(&*ctx, target) {
        return Some(value_cid == proxy);
    }
    let target_cid = ctx.class_id_by_name(target)?;
    // A lambda/method-reference proxy lives outside the loaded hierarchy
    // entirely (`ClassId >= 0x8000_0000`), so no walk can vouch for it.
    if value_cid.as_u32() >= 0x8000_0000 {
        return None;
    }
    let value_name = ctx.class_name_of_id(value_cid)?;
    if ctx.is_class_synthetic_stub(&value_name) {
        return None;
    }
    // A `java.lang.reflect.Proxy` implements its interfaces at RUNTIME. The
    // `aastore` path can be precise here because it has the RECORDED interface
    // set to consult; this one does not, so it declines. By generated-proxy
    // SHAPE, not substring: a user `Outer$ProxyConfig` is judged (round 13
    // wave 7, lane predicates).
    if crate::lang_class::reflective_name_is_runtime_proxy(&value_name) {
        return None;
    }
    let target_owned = target.to_string();
    if value_cid == target_cid
        || ctx.is_subclass(value_cid, target_cid)
        || ctx.synthetic_implements_declared(value_cid, &target_owned)
    {
        return Some(true);
    }
    // Last, and the only clause that may say NO about an interface: the
    // loader-blind by-name walk. `None` from the context (a mock, a harness
    // with no VM hierarchy) means "cannot tell" and must stay an allow.
    ctx.class_assignable_to_name(value_cid, &target_owned)
}

/// May the non-null `obj` be cast to the ARRAY type `target_desc`? The cast
/// `asType` (and so every adapter door) makes to an array-typed parameter,
/// receiver or return (interpreter round i2 wave 60, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// an `Integer[]` reached a `String[]` parameter through `asType`, a spreader
/// and `invoke`, where HotSpot throws `Cannot cast [Ljava.lang.Integer; to
/// [Ljava.lang.String;`.
///
/// Same contract as [`reference_arg_admitted`]: `Some(false)` is a positive
/// mismatch, `None` "cannot tell". The heap shape settles a non-array and a
/// primitive array ([`vh_array_field_value_admitted`]); a reference array
/// whose header names exactly the target's component is admitted with one
/// read-locked name compare (the common case: a `String[]` into `String[]`,
/// an `int[][]` into `int[][]` -- the header of a multi-dimensional array
/// names its component array class); every other reference array takes the
/// VM's `checkcast` predicate (`array_value_casts_to_desc`), which answers
/// "cannot tell" for a name several loaders define and for a context with no
/// VM behind it.
pub(crate) fn reference_array_target_admitted(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    target_desc: &str,
) -> Option<bool> {
    if ctx.object_is_array(obj) && ctx.heap_element_type_of(obj) == ArrayElementType::Reference {
        let component = &target_desc[1..];
        let want = component
            .strip_prefix('L')
            .and_then(|c| c.strip_suffix(';'))
            .unwrap_or(component);
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(obj))
            .as_deref()
            == Some(want)
        {
            return Some(true);
        }
        // A target whose innermost class the loader-blind index cannot name
        // is not judged (fails open): the varargs repair's own array of such
        // a component (`build_varargs_array`) is allocated with a stand-in
        // component when the by-name resolution fails.
        let innermost = target_desc.trim_start_matches('[');
        if let Some(name) = innermost
            .strip_prefix('L')
            .and_then(|c| c.strip_suffix(';'))
        {
            if name != "java/lang/Object" && ctx.class_id_by_name(name).is_none() {
                return None;
            }
        }
    }
    vh_array_field_value_admitted(ctx, obj, target_desc)
}

/// The `ClassCastException` for a positively-mismatched reference, worded the
/// way HotSpot words it (`Cannot cast java.lang.Integer to java.lang.String`).
pub(crate) fn reference_cast_failure(
    ctx: &mut dyn NativeContext,
    value: Value,
    target_desc: &str,
) -> MethodCallFailed {
    let from = match value {
        // An array's class id is its COMPONENT's; name the array itself, the
        // way HotSpot does (`Cannot cast [LH; to H`).
        Value::Object(Some(o)) if ctx.object_is_array(o) => entry_value_type_display(&*ctx, o),
        Value::Object(Some(o)) => {
            let cid = ctx.class_id_of_object(o);
            ctx.class_name_of_id(cid).unwrap_or_default()
        }
        _ => String::new(),
    };
    let to = target_desc
        .strip_prefix('L')
        .and_then(|t| t.strip_suffix(';'))
        .unwrap_or(target_desc);
    // A lambda proxy's `getName()` keeps the `/` before its `0x` tail (wave 92).
    let to = match to.find("$$Lambda/0x") {
        Some(at) => format!("{}{}", to[..at].replace('/', "."), &to[at..]),
        None => to.class_text(&*ctx),
    };
    RuntimeError::ClassCastException {
        message: format!("Cannot cast {} to {to}", from.class_text(&*ctx)),
    }
    .into()
}

/// Kill switch for `asType`'s reference -> reference cast at a LEAF handle and
/// at an adapter's `invoke` entry (interpreter round i1 wave 27, lane L4;
/// [`leaf_reference_refusal`]). `false` restores wave 26, where only the
/// `invoke` / `invokeWithArguments` doors of a DIRECT handle judged a
/// reference (`invoke_reference_cast_refusal`).
pub(crate) const LEAF_CASTS_REFERENCES: bool = true;

/// `asType`'s reference -> reference step for one argument on its way to the
/// parameter (or receiver type) `pdesc` of a LEAF handle, or to the declared
/// parameter of an adapter entered through `invoke`: the `ClassCastException`
/// HotSpot raises for a non-null reference that is positively not a `pdesc`,
/// else `None` (interpreter round i1 wave 27, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// Wave 26 made the PRIMITIVE half of this conversion fallible at the leaf;
/// the reference half was a pass-through, so a reference an adapter bound,
/// dropped past, spread or passed on reached the leaf uncast: a field
/// setter wrote an `int` into the slot of a `String` receiver, a getter read
/// one, a `String` field received an `Integer`, and a static method's
/// `String` parameter held an `Integer` its bytecode then used as a `String`.
///
/// Fails OPEN, as every reference check in this file does: `null`, an
/// `Object` parameter, a primitive parameter (not this function's question),
/// a class the loader-blind unique-name index (`class_id_by_name`) cannot
/// name, a lambda or `java.lang.reflect.Proxy` value, a VM stand-in -- all
/// pass ([`reference_arg_admitted`]'s rules). What IS refused: a non-array at
/// an array parameter, a primitive array at any other array type (`String[]`
/// into `int[]`, `int[]` into `Object[]`), a REFERENCE array whose component
/// the VM's `checkcast` predicate positively refuses (`Integer[]` into
/// `String[]`, wave 60: [`reference_array_target_admitted`]; until then its
/// component was never asked), and a reference the hierarchy positively
/// contradicts.
///
/// Cost, in the order the checks run: an exact class-name match (one
/// read-locked `Arc<str>` clone) answers the common case; a subclass or an
/// interface implementor is admitted by one `class_id_by_name` + one
/// `is_subclass`; only a value neither admits pays [`reference_arg_admitted`].
///
/// `explicit` is [`mh_explicit_scope`]: `explicitCastArguments` passes a
/// reference to an INTERFACE parameter without a cast (its javadoc: "the
/// value of type T0 is passed as a T1 without a cast"), so an interface
/// target is not judged under it; a class target still casts.
pub(crate) fn leaf_reference_refusal(
    ctx: &mut dyn NativeContext,
    value: Value,
    pdesc: &str,
    explicit: bool,
) -> Option<MethodCallFailed> {
    if !LEAF_CASTS_REFERENCES {
        return None;
    }
    let Value::Object(Some(obj)) = value else {
        return None;
    };
    if pdesc.starts_with('[') {
        // A reference array is judged by its component too (wave 60): an
        // `Integer[]` is no `String[]`. An array type is no interface, so the
        // explicit scope casts it as well.
        if reference_array_target_admitted(&*ctx, obj, pdesc) == Some(false) {
            return Some(reference_cast_failure(ctx, value, pdesc));
        }
        return None;
    }
    let target = pdesc.strip_prefix('L')?.strip_suffix(';')?;
    if target.is_empty() || target == "java/lang/Object" {
        return None;
    }
    let is_array = ctx.object_is_array(obj);
    if !is_array {
        // An array's header carries its COMPONENT's class id, so neither fast
        // path may admit one: `reference_arg_admitted` settles arrays.
        let value_cid = ctx.class_id_of_object(obj);
        if ctx.class_name_arc_of_id(value_cid).as_deref() == Some(target) {
            return None;
        }
        if let Some(target_cid) = ctx.class_id_by_name(target) {
            if ctx.is_subclass(value_cid, target_cid) {
                return None;
            }
            if explicit && ctx.is_interface_class(target_cid) {
                return None;
            }
        } else if explicit && lambda_proxy_descriptor_class(&*ctx, target).is_none() {
            // An unresolvable target is not judged below either. A lambda
            // proxy target IS judged (interpreter round i4 wave 93, lane L4;
            // `L4W93ExplicitCastToProxy`): it is a final hidden CLASS, never
            // an interface, so `explicitCastArguments` casts to it as HotSpot's
            // `Class.cast` does, and `reference_arg_admitted` answers it by id.
            return None;
        }
    } else if explicit
        && ctx
            .class_id_by_name(target)
            .is_some_and(|cid| ctx.is_interface_class(cid))
    {
        return None;
    }
    if reference_arg_admitted(ctx, value, pdesc) == Some(false) {
        return Some(reference_cast_failure(ctx, value, pdesc));
    }
    None
}

/// [`leaf_reference_refusal`] for the RECEIVER of an unbound leaf handle
/// (`VIRTUAL`, `SPECIAL`, an instance `GETTER` / `SETTER`), whose type is the
/// handle's `MH_CLASS` (an internal class name, or an array descriptor for an
/// array class) -- `DirectMethodHandle.checkReceiver` / `checkBase` on
/// HotSpot. The exact-class case is answered before a descriptor is built.
pub(crate) fn leaf_receiver_refusal(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    class: &str,
    explicit: bool,
) -> Option<MethodCallFailed> {
    if !LEAF_CASTS_REFERENCES || class.is_empty() || class == "java/lang/Object" {
        return None;
    }
    let value = Value::Object(Some(receiver));
    if class.starts_with('[') {
        return leaf_reference_refusal(ctx, value, class, explicit);
    }
    if !ctx.object_is_array(receiver)
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(receiver))
            .as_deref()
            == Some(class)
    {
        return None;
    }
    leaf_reference_refusal(ctx, value, &format!("L{class};"), explicit)
}

/// The refusal a `SPECIAL` leaf raises for a receiver [`leaf_receiver_refusal`]
/// refused against `MH_CLASS`, reworded to name the handle's own receiver type
/// -- `findSpecial`'s `specialCaller`, the first parameter of its `type` --
/// as HotSpot's cast does. Read only on the refusal path. `fallback` (the
/// `MH_CLASS` wording) stands when the handle has no readable `type`, or its
/// first parameter is not a class.
pub(crate) fn special_receiver_refusal_named(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    receiver: ObjectRef,
    fallback: MethodCallFailed,
) -> MethodCallFailed {
    let first = mh_declared_descriptor(ctx, mh)
        .and_then(|d| split_descriptor_params(&d))
        .and_then(|(params, _)| params.into_iter().next());
    match first {
        Some(p) if p.starts_with('L') => {
            reference_cast_failure(ctx, Value::Object(Some(receiver)), &p)
        }
        _ => fallback,
    }
}

/// `MethodHandle.invoke` is `asType(callSiteType)` then an exact invocation, and
/// `asType` CASTS every reference argument to the handle's declared parameter
/// type. This VM passed them through untouched.
///
/// Measured, `probes/L5ModuleInvokeSweep.java` and a two-line reduction:
///
/// ```text
/// cat  = findVirtual(String, "concat", (String)String)
/// cat.invoke("ab", (Object) Integer.valueOf(3))
///   HotSpot   ClassCastException: Cannot cast java.lang.Integer to java.lang.String
///   CratonVM  NoSuchMethodError: 'boolean java.lang.Integer.isEmpty()'
///
/// len  = findVirtual(String, "length", ()int)
/// len.invoke((Object) Integer.valueOf(3))
///   HotSpot   ClassCastException: Cannot cast java.lang.Integer to java.lang.String
///   CratonVM  NoSuchMethodError: 'int java.lang.Integer.length()'
/// ```
///
/// The messages name the mechanism exactly: the `Integer` reached the callee and
/// the callee's own body then dispatched `isEmpty()` / `length()` on it.
/// **The RECEIVER is uncast too, not only the parameters.**
///
/// `NoSuchMethodError` is not a smaller version of the same behaviour. It
/// extends `Error`, so a `catch (ClassCastException)` -- or any
/// `catch (RuntimeException)` around a reflective dispatch -- does not see it,
/// and the name it carries belongs to a method the caller never wrote.
///
/// `MH_DESC` omits the receiver for virtual/special handles (`needs_receiver`),
/// so the receiver is judged against `MH_CLASS` (a special handle's recorded
/// `specialCaller`, [`mh_leaf_receiver_class`]) and the rest 1:1 against the
/// descriptor -- the same split `adapt_invoke_args` uses a few lines below.
pub(crate) fn invoke_reference_cast_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    needs_receiver: bool,
    has_bound: bool,
    extra: &[Value],
    call_site: Option<&str>,
) -> Option<MethodCallFailed> {
    if !kind_has_authoritative_type(kind) {
        return None;
    }
    let declared = mh_declared_descriptor(ctx, mh);
    invoke_reference_cast_refusal_with(
        ctx,
        mh,
        kind,
        needs_receiver,
        has_bound,
        extra,
        call_site,
        declared.as_deref(),
    )
}

/// [`invoke_reference_cast_refusal`] with the handle's declared type
/// (`mh_declared_descriptor`) already read: the doors read it once per call
/// (round 12 wave 7) and hand it to every check that needs it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn invoke_reference_cast_refusal_with(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    needs_receiver: bool,
    has_bound: bool,
    extra: &[Value],
    call_site: Option<&str>,
    declared_type: Option<&str>,
) -> Option<MethodCallFailed> {
    // ONLY a handle whose `MH_DESC` is authoritative for its own arguments.
    //
    // An ADAPTER -- `filterArguments`, `insertArguments`, `foldArguments`, a
    // spread/collect, the `__adapter__` a second `bindTo` mints -- keeps the
    // LEAF member's descriptor in `MH_DESC` while presenting a different
    // parameter list to its caller. `filterArguments(cat, 1, intToString)` has
    // the same ARITY as `cat`, so the length guard below cannot see it, and
    // comparing the caller's `Integer` against the leaf's
    // `Ljava/lang/String;` would refuse a perfectly correct call.
    //
    // `kind_has_authoritative_type` stays on the narrow, direct-kinds-only
    // list for exactly the reason above -- this function reads `MH_DESC`, and
    // an adapter's `MH_DESC` is its leaf's, not its own. `exact_call_site_
    // refusal` and `invoke_narrowing_arg_refusal` read `type` instead (via
    // `mh_declared_descriptor`), which IS adapted for the combinator surface,
    // so they use the wider `kind_has_authoritative_adapted_type`. Widening
    // THIS gate to match would reintroduce the false refusal described above.
    if !kind_has_authoritative_type(kind) {
        return None;
    }
    let desc = mh_read_desc(ctx, mh)?;
    let (params, _) = split_descriptor_params(&desc)?;
    invoke_reference_cast_refusal_decoded(
        ctx,
        mh,
        kind,
        needs_receiver,
        has_bound,
        extra,
        call_site,
        declared_type,
        &params,
        None,
    )
}

/// [`invoke_reference_cast_refusal_with`] once `MH_DESC` is decoded and split
/// (`params`), and, when the caller already holds it, `MH_CLASS` decoded
/// (`class`; `None` reads it here, as before). Round 15 wave 1 (lane mh): the
/// direct lane hands in its [`DirectLaneRecord`]'s, decoded from the same
/// objects, so the `invoke` door's refusals decode nothing per call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn invoke_reference_cast_refusal_decoded(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    needs_receiver: bool,
    has_bound: bool,
    extra: &[Value],
    call_site: Option<&str>,
    declared_type: Option<&str>,
    params: &[String],
    class: Option<&str>,
) -> Option<MethodCallFailed> {
    // See `invoke_reference_cast_refusal_with`: direct kinds only.
    if !kind_has_authoritative_type(kind) {
        return None;
    }
    // The OUTER cast first -- `asType(callSiteType)` casts to the handle's
    // declared `type`, receiver included -- then the leaf's own below.
    let declared = match invoke_declared_type_refusal(ctx, mh, declared_type, extra, call_site) {
        Ok(declared) => declared,
        Err(refusal) => return Some(refusal),
    };
    let receiver_taken = needs_receiver && !has_bound && !extra.is_empty();
    if receiver_taken {
        let class = match class {
            Some(class) => Some(class.to_string()),
            None => mh_read_class(ctx, mh),
        };
        if let Some(class) = class {
            let class = mh_leaf_receiver_class(ctx, mh, class);
            // `findVirtual(int[].class, "clone", ..)` names an ARRAY class,
            // whose descriptor is its name: `L[I;` read as the class `[I`,
            // which no array is an instance of, and refused the call.
            let target = if class.starts_with('[') {
                class
            } else {
                format!("L{class};")
            };
            let judged = declared_position_judged(declared.as_deref(), 0, &target);
            if !judged && reference_arg_admitted(ctx, extra[0], &target) == Some(false) {
                let refusal = reference_cast_failure(ctx, extra[0], &target);
                // A `findSpecial` handle's receiver type is its
                // `specialCaller`, which HotSpot's message names (wave 27).
                // `CRATONVM_MH_SPECIAL_CALLER_CAST=0` restores the leaf-only
                // cast (round 12 wave 5), message included.
                if let (MH_KIND_SPECIAL, Value::Object(Some(r)), true) =
                    (kind, extra[0], special_caller_cast_enabled())
                {
                    return Some(special_receiver_refusal_named(ctx, mh, r, refusal));
                }
                return Some(refusal);
            }
        }
    }
    let rest = if receiver_taken { &extra[1..] } else { extra };
    // Only a 1:1 alignment is judged. A varargs collector, a spread/collect
    // adapter or a partially bound handle can legitimately present a different
    // arity here, and guessing the alignment is how a cast check starts refusing
    // correct calls.
    if rest.len() != params.len() {
        return None;
    }
    // The call site's static parameter types, aligned with `extra` (the
    // `invoke` descriptor names every argument after the handle, the receiver
    // included), when the door published one that fits.
    let site_params = call_site
        .and_then(split_descriptor_params)
        .map(|(p, _)| p)
        .filter(|p| p.len() == extra.len());
    let offset = usize::from(receiver_taken);
    // A handle `explicitCastArguments` stamped reaches a primitive `MH_DESC`
    // parameter through a JLS 5.5 cast (a `null` is zero, a `Long` narrows),
    // not `asType`'s conversion: its primitive positions are not judged here
    // (wave 26; `MH_MARK_EXPLICIT_CAST`).
    let explicit = mh_is_explicit_cast(&*ctx, mh);
    for (i, (v, pdesc)) in rest.iter().zip(params.iter()).enumerate() {
        if wrapper_desc_for_primitive(pdesc).is_some() {
            if explicit {
                continue;
            }
            // A REFERENCE reaching a primitive parameter is `asType`'s
            // reference -> primitive conversion (wave 25): a `String`, a
            // non-widening wrapper or a `null` is refused, where
            // `adapt_single_arg` used to pass it through to the callee
            // (`findStatic(C, "m", (int)void).invoke((Object) "s")` ran `m`
            // on the String's address). A site that typed the argument as a
            // primitive passed it raw; a reference there is ours, not the
            // caller's, and is left to `adapt_single_arg` as before.
            if matches!(v, Value::Object(_)) {
                let site_param = site_params
                    .as_ref()
                    .and_then(|p| p.get(i + offset))
                    .map(String::as_str);
                if site_param.is_some_and(|p| wrapper_desc_for_primitive(p).is_some()) {
                    continue;
                }
                if let Err(refusal) = mh_unbox_for_primitive(ctx, pdesc, *v, site_param) {
                    return Some(refusal);
                }
            }
            continue;
        }
        // Already judged by the declared-type pass above (round 12 wave 5).
        if declared_position_judged(declared.as_deref(), offset + i, pdesc) {
            continue;
        }
        // `explicitCastArguments` passes a reference to an INTERFACE parameter
        // without a cast (`VerifyType.isNullConversion(.., keepInterfaces =
        // false)`; the leaf arms skip it under the explicit scope since wave
        // 27), so this door must not refuse it either (round 12 wave 7, lane
        // mh5; hunter3's residual 1): `explicitCastArguments(takesRunnable,
        // (Object)int).invoke("x")` runs the target on HotSpot.
        if explicit
            && descriptor_names_an_interface(&*ctx, pdesc)
            && explicit_interface_uncast_enabled()
        {
            continue;
        }
        if reference_arg_admitted(ctx, *v, pdesc) == Some(false) {
            if i + 1 == rest.len() && varargs_collects_last(&*ctx, mh, call_site, declared_type) {
                continue;
            }
            return Some(reference_cast_failure(ctx, *v, pdesc));
        }
    }
    None
}

/// Does `mh`, a varargs collector, COLLECT its trailing argument at this
/// `invoke` site rather than take it as the array? `AsVarargsCollector.asType`
/// passes the trailing argument through as the array only when the site's
/// type for it is assignable to the array type
/// ([`call_site_names_trailing_array`]); otherwise the argument is an ELEMENT,
/// cast to the component by the collection (`mh_entry_adapt`), so the array
/// cast must not judge it (interpreter round i2 wave 60, lane L4: once array
/// arguments were judged, `varargs.invoke((Object) new Integer[1])` refused
/// with `Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;` where
/// HotSpot collects and refuses the element, `... to java.lang.String`).
/// Asked only on a refusal's path. No site, or no declared type: `true`
/// (never refuse on a guess).
pub(crate) fn varargs_collects_last(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    call_site: Option<&str>,
    declared: Option<&str>,
) -> bool {
    if !mh_is_varargs_collector(ctx, mh) {
        return false;
    }
    match (call_site, declared) {
        (Some(site), Some(declared)) => !call_site_names_trailing_array(Some(site), declared),
        _ => true,
    }
}

/// `CRATONVM_MH_EXPLICIT_INTERFACE_UNCAST` -- default ON (round 12 wave 7,
/// lane mh5); `0`/`false`/`off`/`no` makes the `invoke` /
/// `invokeWithArguments` doors cast a reference to an interface parameter of
/// an `explicitCastArguments` copy again (a `ClassCastException` HotSpot does
/// not raise). Read only for an explicit copy's interface parameter.
pub(crate) fn explicit_interface_uncast_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_EXPLICIT_INTERFACE_UNCAST")
}

/// The cast `asType(callSiteType)` makes to the handle's DECLARED `type`, for
/// `invoke` and `invokeWithArguments`: `Err` is the `ClassCastException`,
/// `Ok(Some(params))` the declared parameters every argument was judged
/// against, `Ok(None)` "not judged" (no readable `type`, or an arity that is
/// not the argument count -- a collector, a fabricated `()V`).
///
/// The leaf check in [`invoke_reference_cast_refusal`] reads `MH_CLASS` and
/// `MH_DESC`, and for a `findSpecial` / `unreflectSpecial` handle those are
/// NOT the declared type: the JDK narrows the receiver to `specialCaller`
/// (`Lookup.restrictReceiver`), while `MH_CLASS` stays the class dispatch
/// resolves in. Measured on round 12 wave 4 (`R12Hunt2FindSpecial`):
/// `findSpecial(Mid, "m", (int)int, Sub).invoke((Object) new Mid(), 3)` ran
/// `Mid.m` on a `Mid` and answered 9; HotSpot 25 throws
/// `ClassCastException: Cannot cast Mid to Sub`. The same pass also covers an
/// `asType` copy of a direct handle, whose `type` is the outer type the call
/// site is cast to while `MH_DESC` keeps the leaf's.
///
/// Same predicate, same fail-open rule as every other cast in this file:
/// only a positive `Some(false)` from [`reference_arg_admitted`] refuses.
/// `CRATONVM_MH_INVOKE_DECLARED_CAST=0` restores the leaf-only check; it is
/// read only when a refusal is about to be raised, so the common path pays no
/// flag read. Spec conformance, identical in both modes.
pub(crate) fn invoke_declared_type_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    declared: Option<&str>,
    extra: &[Value],
    call_site: Option<&str>,
) -> Result<Option<Vec<String>>, MethodCallFailed> {
    // `declared` is the handle's `mh_declared_descriptor` (round 12 wave 7:
    // read once per call by the door).
    let Some(declared_desc) = declared else {
        return Ok(None);
    };
    let Some(params) = declared_cast_params(declared_desc, extra.len()) else {
        return Ok(None);
    };
    for (pos, (v, pdesc)) in extra.iter().zip(params.iter()).enumerate() {
        if reference_arg_admitted(ctx, *v, pdesc) == Some(false) {
            if !invoke_declared_cast_enabled() {
                return Ok(None);
            }
            if call_site_names_declared_param(call_site, extra.len(), pos, pdesc) {
                continue;
            }
            if pos + 1 == extra.len() && varargs_collects_last(&*ctx, mh, call_site, declared) {
                continue;
            }
            return Err(reference_cast_failure(ctx, *v, pdesc));
        }
    }
    Ok(Some(params))
}

/// Round 13 wave 1 (lane mhffm; `r12w5-hunter3-methodhandle-cast-model-residuals`
/// item 2): does the call site already type argument `pos` exactly as the
/// declared parameter `pdesc`? `asType(callSiteType)` casts only where the two
/// types differ, so HotSpot does not judge such a position (only non-javac
/// bytecode can put a non-implementer in an interface-typed slot there).
/// `CRATONVM_MH_INVOKE_DECLARED_CAST_SKIPS_TYPED=0` judges it anyway, as
/// before; read only when a refusal is about to be raised.
pub(crate) fn call_site_names_declared_param(
    call_site: Option<&str>,
    argc: usize,
    pos: usize,
    pdesc: &str,
) -> bool {
    let Some((site, _)) = call_site.and_then(split_descriptor_params) else {
        return false;
    };
    site.len() == argc
        && site.get(pos).is_some_and(|t| t == pdesc)
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_MH_INVOKE_DECLARED_CAST_SKIPS_TYPED",
        )
}

/// `CRATONVM_MH_INVOKE_DECLARED_CAST` -- default ON (round 12 wave 5, lane
/// hunter3); `0`/`false`/`off`/`no` restores the leaf-only cast
/// (`MH_CLASS`/`MH_DESC`) that `invoke` performed before.
pub(crate) fn invoke_declared_cast_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_DECLARED_CAST")
}

/// The declared parameters of `declared` when there is one per argument, or
/// `None`. `argc == 0` has nothing to cast.
pub(crate) fn declared_cast_params(declared: &str, argc: usize) -> Option<Vec<String>> {
    if argc == 0 {
        return None;
    }
    let (params, _) = split_descriptor_params(declared)?;
    (params.len() == argc).then_some(params)
}

/// Did the declared pass already judge argument `pos` against this very
/// `target`? Then the leaf pass would ask the same question of the same value.
pub(crate) fn declared_position_judged(declared: Option<&[String]>, pos: usize, target: &str) -> bool {
    declared
        .and_then(|d| d.get(pos))
        .is_some_and(|t| t == target)
}

/// The INNER cast of an `asType` copy of a direct handle, which
/// `invokeExact` owes as much as `invoke` does, or `None`.
///
/// `h.asType(t)` is a new handle whose adapter casts each argument from `t`'s
/// parameter to the target's; `invokeExact` on it performs that cast. This VM
/// stamps `t` on a copy of the direct handle (`mh_with_stamped_type`), so the
/// adapter is gone and `invokeExact` dispatched the uncast argument straight
/// to the leaf. Measured shapes (HotSpot 25: `ClassCastException` for each):
///
/// ```text
/// findVirtual(Mid, m, (int)int).asType((Object,int)int).invokeExact((Object) grand, 3)
///     ran Grand.m on the Grand
/// findStatic(C, takesSub, (Sub)int).asType((Object)int).invokeExact((Object) mid)
///     ran takesSub with a Mid in its Sub parameter
/// ```
///
/// The second is not only a wrong answer: the callee's bytecode, and its
/// compiled code, trust a verified `Sub` parameter, so a field of `Sub` read
/// through it lands outside a `Mid`.
///
/// Judged exactly where the declared parameter and the leaf's differ, which is
/// exactly where the JDK's adapter casts; an unadapted handle (declared ==
/// leaf at every position) is never judged. Same predicate and fail-open rule
/// as [`invoke_reference_cast_refusal`]'s leaf pass, and the same leaf
/// placement: [`mh_leaf_receiver_class`] (`specialCaller`, else `MH_CLASS`)
/// for an unbound virtual/special receiver, `MH_DESC` for the rest. So
/// `findSpecial(Mid, m, (int)int, Sub).asType((Object,int)int)
/// .invokeExact((Object) mid, 3)` is refused as on HotSpot, although the copy's
/// `type` no longer names `Sub`.
/// `CRATONVM_MH_INVOKEEXACT_INNER_CAST=0` restores the uncast dispatch; it is
/// read only when a refusal is about to be raised.
pub(crate) fn invoke_exact_inner_cast_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    declared: Option<&str>,
    extra: &[Value],
) -> Option<MethodCallFailed> {
    // A real JDK handle has no `MH_BOUND` / `MH_DESC` to judge (it would end
    // at `mh_read_desc`'s `None`; the reads were heap OOB warnings; wave 56,
    // lane L4).
    if !kind_has_authoritative_type(kind)
        || extra.is_empty()
        || ctx.object_num_fields(mh) <= MH_BOUND
    {
        return None;
    }
    let (declared_params, _) = split_descriptor_params(declared?)?;
    if declared_params.len() != extra.len() {
        return None;
    }
    let bound = matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)));
    let mut leaf: Vec<String> = Vec::with_capacity(extra.len());
    if matches!(kind, MH_KIND_VIRTUAL | MH_KIND_SPECIAL) && !bound {
        let class = mh_read_class(ctx, mh)?;
        let class = mh_leaf_receiver_class(ctx, mh, class);
        leaf.push(if class.starts_with('[') {
            class
        } else {
            format!("L{class};")
        });
    }
    let desc = mh_read_desc(ctx, mh)?;
    let (leaf_params, _) = split_descriptor_params(&desc)?;
    // A bound STATIC handle's `MH_DESC` still names the captured leading
    // parameter, which the caller does not pass.
    let skip = usize::from(kind == MH_KIND_STATIC && bound);
    leaf.extend(leaf_params.into_iter().skip(skip));
    if leaf.len() != extra.len() {
        return None;
    }
    // An `explicitCastArguments` stamp passes a reference to an INTERFACE
    // uncast (JLS 5.5; `leaf_reference_refusal`'s rule, which the leaf arms
    // apply): `explicitCastArguments(takesRunnable, (Object)String)
    // .invokeExact((Object) "s")` answers `s` on HotSpot 25.0.3, and this
    // refused it with `Cannot cast java.lang.String to java.lang.Runnable`
    // (interpreter round i1 wave 28, lane L4, reviewing round 12's cast).
    let explicit = mh_is_explicit_cast(&*ctx, mh);
    for ((v, have), want) in extra.iter().zip(declared_params.iter()).zip(leaf.iter()) {
        if have == want {
            continue;
        }
        if explicit && descriptor_names_an_interface(&*ctx, want) {
            continue;
        }
        if reference_arg_admitted(ctx, *v, want) == Some(false) {
            // An `explicitCastArguments` copy passes a reference to an
            // INTERFACE parameter uncast (round 12 wave 7, lane mh5; the
            // `invoke` door's leaf pass skips it for the same reason).
            if mh_is_explicit_cast(&*ctx, mh)
                && descriptor_names_an_interface(&*ctx, want)
                && explicit_interface_uncast_enabled()
            {
                continue;
            }
            if !invoke_exact_inner_cast_enabled() {
                return None;
            }
            return Some(reference_cast_failure(ctx, *v, want));
        }
    }
    None
}

/// `CRATONVM_MH_INVOKEEXACT_INNER_CAST` -- default ON (round 12 wave 5, lane
/// hunter3); `0`/`false`/`off`/`no` restores `invokeExact`'s uncast dispatch
/// of an `asType` copy of a direct handle.
pub(crate) fn invoke_exact_inner_cast_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKEEXACT_INNER_CAST")
}

/// The reference RETURN cast an `asType` adapter owes, or `None`.
///
/// The argument casts above have a return-side twin that nothing performed:
/// `(String) mh.invoke()` on a handle declared `()Object`, or `invokeExact` on
/// `findStatic(C, "obj", ()Object).asType(()String)`, handed whatever object
/// the leaf returned straight to a call site typed `String` -- javac emits no
/// `checkcast` after a signature-polymorphic call (the cast is in the call
/// site's descriptor), and `vm_exec::unbox_poly_return` passes references
/// through, so the caller held, say, an `Integer` in a `String` variable.
/// HotSpot 25 throws `ClassCastException` from the adapter.
///
/// Two casts, exactly where the JDK's adapters make them:
/// * INNER -- an `asType` copy of a direct handle (`kind_has_authoritative_type`)
///   whose declared return differs from the leaf's reference return;
/// * OUTER -- `invoke`'s `asType(callSiteType)`, when the call site's return
///   differs from the declared (else leaf) reference return.
///
/// Class, interface and (since wave 60) array targets are judged. Only the
/// INNER cast of an `explicitCastArguments` stamp (`explicit`, read by the
/// door before its dispatch) skips an interface: that conversion is JLS 5.5's,
/// which passes a reference to an interface uncast. `asType` casts to one --
/// HotSpot 25.0.3: `(Runnable) h.invoke()` on a `()Object` handle answering a
/// `String`, and `h.asType(()Runnable).invokeExact()`, throw `Cannot cast
/// java.lang.String to java.lang.Runnable` -- and round 12 skipped every
/// interface, so the caller held the `String` in a `Runnable` variable
/// (interpreter round i1 wave 28, lane L4). A primitive leaf return is not
/// this function's (boxing to an unrelated class is `asType`'s
/// `WrongMethodTypeException`). Same predicate and fail-open rule as the
/// argument casts. `CRATONVM_MH_RETURN_CAST=0` restores the uncast return; it
/// is read only when a refusal is about to be raised.
pub(crate) fn return_cast_refusal(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    site: Option<&str>,
    explicit: bool,
) -> Option<MethodCallFailed> {
    let value = match result {
        Ok(Some(v @ Value::Object(Some(_)))) => *v,
        _ => return None,
    };
    let is_reference = |t: &str| t.starts_with('L') || t.starts_with('[');
    let leaf_ret = return_type_desc(leaf_desc);
    let declared_ret = declared.map(return_type_desc);
    let inner = declared_ret
        .filter(|d| kind_has_authoritative_type(kind) && is_reference(leaf_ret) && *d != leaf_ret);
    let from = declared_ret.unwrap_or(leaf_ret);
    let outer = site
        .map(return_type_desc)
        .filter(|s| is_reference(from) && *s != from);
    let targets = [inner.map(|t| (t, true)), outer.map(|t| (t, false))];
    for (target, is_inner) in targets.into_iter().flatten() {
        // An array target is cast too, component included (wave 60, lane
        // L4: `asType(()String[])` over a leaf returning an `Integer[]` as
        // `Object` handed the `Integer[]` to a `String[]` call site).
        if !is_reference(target) {
            continue;
        }
        if is_inner && explicit && descriptor_names_an_interface(ctx, target) {
            continue;
        }
        if reference_arg_admitted(ctx, value, target) == Some(false) {
            if !return_cast_enabled() {
                return None;
            }
            return Some(reference_cast_failure(ctx, value, target));
        }
    }
    None
}

/// Could [`return_cast_refusal`] judge a cast for these types? A door that owes
/// one does work after its dispatch, so it must not arm [`MH_TAIL`]: a yield
/// that unwound a transparent door frame would skip the cast. The same inner /
/// outer targets as [`return_cast_refusal`], without its interface screen, so
/// the answer errs towards an opaque door.
pub(crate) fn return_cast_may_apply(
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    site: Option<&str>,
) -> bool {
    let is_reference = |t: &str| t.starts_with('L') || t.starts_with('[');
    let leaf_ret = return_type_desc(leaf_desc);
    let declared_ret = declared.map(return_type_desc);
    let inner = declared_ret
        .filter(|d| kind_has_authoritative_type(kind) && is_reference(leaf_ret) && *d != leaf_ret);
    let from = declared_ret.unwrap_or(leaf_ret);
    let outer = site
        .map(return_type_desc)
        .filter(|s| is_reference(from) && *s != from);
    [inner, outer].into_iter().flatten().any(is_reference)
}

/// Does the class descriptor `desc` (`Lp/C;`) name a loaded INTERFACE? `false`
/// when it names no loaded class.
pub(crate) fn descriptor_names_an_interface(ctx: &dyn NativeContext, desc: &str) -> bool {
    desc.strip_prefix('L')
        .and_then(|d| d.strip_suffix(';'))
        .and_then(|name| ctx.class_id_by_name(name))
        .is_some_and(|cid| ctx.is_interface_class(cid))
}

/// `CRATONVM_MH_RETURN_CAST` -- default ON (round 12 wave 5, lane hunter3);
/// `0`/`false`/`off`/`no` restores the uncast reference return of `invoke` /
/// `invokeExact`.
pub(crate) fn return_cast_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_RETURN_CAST")
}

/// The reference -> primitive RETURN conversion an `asType` adapter owes, or
/// `None` (the door's ordinary boxing then runs, as before). The primitive
/// twin of [`return_cast_refusal`] (interpreter round i2 wave 58, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// `target` is the type the door's caller receives: the call site for
/// `invoke`, the declared type for `invokeExact`. Two conversions, exactly
/// where the JDK's adapters make them:
/// * OUTER -- `invoke`'s `asType(callSiteType)` from a handle whose DECLARED
///   return is a reference to a primitive call-site return (`(int)
///   objHandle.invoke()`), for a kind whose `type` is authoritative;
/// * INNER -- an `asType` copy of a direct handle
///   (`kind_has_authoritative_type`) from its leaf's reference return to a
///   primitive declared one (`objHandle.asType(()int).invokeExact()`), then
///   widened to `target`'s.
///
/// Measured on HotSpot 25.0.3 (`tools/probes/interp/L4/L4W58MethodHandleReturnUnboxing.java`):
/// `null` is `primitiveConversion`'s NPE, a wrapper that does not widen (or a
/// `Number` that is no wrapper) `Cannot cast java.lang.Long to
/// java.lang.Integer`, anything else the `(Number)` cast's CCE: exactly
/// [`mh_unbox_for_primitive`]'s answers for an argument. The coercion after
/// the native (`vm_exec::coerce_value_against_ret_char`) fabricated `0` for
/// each. A `void` declared return is never judged (`asType` makes it `0`),
/// nor a raw primitive the dispatch did not box, nor a handle with no `type`.
///
/// One predictable branch on the hot path: `target` returns a primitive.
#[inline]
pub(crate) fn invoke_return_unbox(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    marks: i32,
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    target: Option<&str>,
) -> Option<MethodCallResult> {
    let target = target?;
    let bytes = target.as_bytes();
    // `...)I`: a one-byte return after the parameter list; `)[I` and `;)` do
    // not match, `V` is `asType`'s zero.
    if bytes.len() < 2 || bytes[bytes.len() - 2] != b')' || bytes[bytes.len() - 1] == b'V' {
        return None;
    }
    invoke_return_unbox_cold(ctx, result, marks, kind, leaf_desc, declared?, target)
}

/// Could [`invoke_return_unbox`]'s INNER conversion apply to a door whose
/// caller receives a REFERENCE (the `invokeWithArguments` doors)? Such a door
/// then does work after its dispatch and must not arm [`MH_TAIL`]'s `L`
/// token (a yield under a transparent door would skip the unboxing). The
/// same shape test, without the value; string compares only.
pub(crate) fn inner_return_unbox_may_apply(
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    marks: i32,
) -> bool {
    let Some(declared) = declared else {
        return false;
    };
    let declared_ret = return_type_desc(declared);
    let leaf_ret = return_type_desc(leaf_desc);
    declared_ret.len() == 1
        && declared_ret != "V"
        && (leaf_ret.starts_with('L') || leaf_ret.starts_with('['))
        && kind_has_authoritative_type(kind)
        && marks & MH_MARK_CAST_CHAIN == 0
        && crate::nbflags().mh_invoke_return_unbox
}

/// The shape test and conversion of [`invoke_return_unbox`], for a `target`
/// with a primitive return.
#[inline(never)]
pub(crate) fn invoke_return_unbox_cold(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    marks: i32,
    kind: i32,
    leaf_desc: &str,
    declared: &str,
    target: &str,
) -> Option<MethodCallResult> {
    let enabled = crate::nbflags().mh_invoke_return_unbox;
    let converted = invoke_return_unbox_judged(
        ctx, result, marks, kind, leaf_desc, declared, target, enabled,
    )?;
    // Boxed as the call site's own wrapper: every route that reaches these
    // natives unboxes it again (`vm_exec::unbox_poly_return_checked`).
    Some(converted.and_then(|v| auto_box_return(ctx, Ok(Some(v)), target)))
}

/// [`invoke_return_unbox_cold`]'s conversion, UNBOXED (the primitive of
/// `target`'s return, or the refusal), with `CRATONVM_MH_INVOKE_RETURN_UNBOX`
/// as a parameter (the unit tests ask both answers).
#[allow(clippy::too_many_arguments)]
pub(crate) fn invoke_return_unbox_judged(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    marks: i32,
    kind: i32,
    leaf_desc: &str,
    declared: &str,
    target: &str,
    enabled: bool,
) -> Option<Result<Value, MethodCallFailed>> {
    if !enabled {
        return None;
    }
    let is_reference = |t: &str| t.starts_with('L') || t.starts_with('[');
    let is_value_primitive = |t: &str| t.len() == 1 && t != "V";
    let target_ret = return_type_desc(target);
    if !is_value_primitive(target_ret) {
        return None;
    }
    let declared_ret = return_type_desc(declared);
    let (from, prim) = if is_reference(declared_ret) && kind_has_authoritative_adapted_type(kind) {
        (declared_ret, target_ret)
    } else if is_value_primitive(declared_ret)
        && kind_has_authoritative_type(kind)
        && is_reference(return_type_desc(leaf_desc))
        // A cast chain judges its own return (`mh_cast_chain_judge_return`).
        && marks & MH_MARK_CAST_CHAIN == 0
    {
        (return_type_desc(leaf_desc), declared_ret)
    } else {
        return None;
    };
    let value = match result {
        Ok(Some(v @ Value::Object(_))) => *v,
        _ => return None,
    };
    // An `explicitCastArguments` stamp's own conversion is JLS 5.5's: a
    // `null` is zero, a wrapper is unboxed and then widened or narrowed
    // (HotSpot 25.0.3: `explicitCastArguments(objLong, ()int).invokeExact()`
    // is `6`; this door's caller refused it with the strict `invokeExact`
    // return check). `invoke`'s own `asType` is never explicit.
    let explicit = from != declared_ret && marks & MH_MARK_EXPLICIT_CAST != 0;
    let converted = if explicit {
        adapt_leaf_arg(ctx, value, prim, Some(from), true)
    } else {
        mh_unbox_for_primitive(ctx, prim, value, Some(from))
    };
    let unboxed = match converted {
        Ok(v) => v,
        Err(refusal) => {
            if crate::nbflags().dbg_mh_dispatch {
                eprintln!("[MH_RETURN_UNBOX] refused {from} -> {prim} at {target}");
            }
            return Some(Err(refusal));
        }
    };
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_RETURN_UNBOX] {from} -> {prim} at {target}");
    }
    let (Some(&p), Some(&t)) = (prim.as_bytes().first(), target_ret.as_bytes().first()) else {
        return None;
    };
    Some(Ok(widen_return_value(unboxed, p, t).unwrap_or(unboxed)))
}

/// `MethodType.genericMethodType(count)`'s `checkSlotCount` for the two
/// `invokeWithArguments` doors: more than 255 arguments are
/// `IllegalArgumentException: bad parameter count <count>` before any
/// conversion, unless the handle is a variable-arity collector, whose own
/// `invokeWithArguments` (`MethodHandleImpl.AsVarargsCollector`) takes a
/// jumbo list without that type. Measured on HotSpot 25
/// (`tools/probes/interp/L4/L4W43InvokeWithArgumentsEdges.java` row
/// `fixed-300`); CratonVM answered the arity's `WrongMethodTypeException`.
/// Interpreter round i1 wave 43, lane L4.
pub(crate) fn invoke_with_arguments_count_refusal(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    count: usize,
) -> Option<MethodCallFailed> {
    if count <= 255 || mh_is_varargs_collector(ctx, this) {
        return None;
    }
    Some(
        RuntimeError::IllegalArgumentException {
            message: format!("bad parameter count {count}"),
        }
        .into(),
    )
}

/// [`invoke_reference_cast_refusal`] for the `invokeWithArguments` doors, which
/// receive their arguments already unpacked and compute `needs_receiver` /
/// `has_bound` for themselves.
///
/// Two registrations spell `invokeWithArguments` -- `([Ljava/lang/Object;)` and
/// `(Ljava/util/List;)` -- and both bypass the `invoke` native entirely. Fixing
/// `invoke` alone left `d.invokeWithArguments` differing, which is the
/// several-independent-doors shape this file meets often enough to have a name
/// for it.
pub(crate) fn invoke_with_arguments_cast_refusal(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    unpacked: &[Value],
    declared: Option<&str>,
) -> Option<MethodCallFailed> {
    // A handle with none of this model's slots (a real JDK handle) has no
    // `MH_KIND` / `MH_BOUND` / `MH_DESC` to judge: its casts are its own
    // `asType`'s (`mh_run_foreign_vmentry` converts its arguments). Reading
    // them was a heap OOB warning per call and a decision taken on defaults
    // (interpreter round i2 wave 56, lane L4;
    // `interpreter-L4-a-door-reads-this-models-slots-of-a-real-handle-FIXED-20261020`).
    if ctx.object_num_fields(this) <= MH_BOUND {
        return None;
    }
    let kind = match ctx.get_field(this, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_VIRTUAL,
    };
    let needs_receiver = kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL;
    let has_bound = matches!(ctx.get_field(this, MH_BOUND), Value::Object(Some(_)));
    // `invokeWithArguments` adapts to `genericMethodType(n)`: every static
    // type is `Object`, which is what `None` means to the primitive check.
    // `declared` is the handle's `mh_declared_descriptor`, read once by the
    // door (round 12 wave 7).
    invoke_reference_cast_refusal_with(
        ctx,
        this,
        kind,
        needs_receiver,
        has_bound,
        unpacked,
        None,
        declared,
    )
}

/// Refuse a `VarHandle` INSTANCE access whose receiver is not an instance of the
/// handle's coordinate class, or whose value does not fit the declared field.
///
/// # What this VM did instead
///
/// `varhandle_set`'s instance arm resolved `field_idx` from the VarHandle's OWN
/// class and then applied it to whatever object arrived, with no arm between
/// the two lines. Measured, `probes/ReflectArgTypeSweep.java`, BOTH modes:
///
/// ```text
/// row                 HotSpot 25.0.3+9           CratonVM
/// v.wrongRef          ClassCastException         3        <- Integer STORED in a String field
/// v.wrongReceiver     ClassCastException         no-throw <- wrote through a String receiver
/// v.primWrongRef      WrongMethodTypeException   no-throw <- "nine" STORED in an int field
/// v.getWrongReceiver  ClassCastException         y        <- READ through a String receiver
/// v.casWrongRef       ClassCastException         true     <- CAS succeeded
/// ```
///
/// Two of those are worse than a wrong exception type. `v.wrongRef` leaves a
/// `String`-declared field holding an `Integer` with nothing failing at the
/// store, so the next ordinary read of that field is where it surfaces -- at a
/// site that did nothing wrong. `v.getWrongReceiver` applied a `Box` field index
/// to a `String` and returned what it found there.
///
/// # Why the ORDER of the checks is the whole design
///
/// These are the CAS-dominated paths this file has been tuned for twice: a
/// thread-local plan memo took the global lock off the JIT's fast paths (a
/// scaling probe went 0.07x -> 0.68x at 24 threads), and `vh_meta_get` returns
/// an `Arc` precisely so a hot op pays a refcount bump instead of three `String`
/// clones. A per-operation `class_id_by_name` + `is_subclass` would take a
/// class-manager read lock on every `CompletableFuture` composition step and
/// undo both.
///
/// So the receiver check is an INTEGER COMPARE against `meta.class_id`, which is
/// already in hand. Only a mismatch -- a subclass receiver, or a genuinely wrong
/// one -- pays `reference_arg_admitted`, and that predicate refuses only on a
/// positive reading. A handle with no meta, or whose meta carries no class name,
/// is not judged at all.
pub(crate) fn vh_instance_refusal(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    receiver: ObjectRef,
    value: Option<Value>,
) -> Option<MethodCallFailed> {
    let meta = meta?;
    if meta.class_name.is_empty() {
        return None;
    }
    // FAST PATH: the overwhelmingly common case is the exact class, and this
    // arm costs two header reads and one integer compare. The array test is
    // part of it because an array's header carries its COMPONENT's class id:
    // an `H[]` receiver matches `meta.class_id` for `H` and, unguarded, had
    // `H`'s field slot applied to the array (r11w13-orch-varhandle-set-on-an-
    // array-receiver-throws-aioobe-not-cce). `reference_arg_admitted` refuses
    // an array against a class target, so the slow path below settles it.
    let recv_cid = ctx.class_id_of_object(receiver);
    if recv_cid.as_u32() != meta.class_id || ctx.object_is_array(receiver) {
        // A lambda proxy's capture (wave 79, `lk_lambda_proxy_var_handle`):
        // the proxy is final and outside the loaded hierarchy, so only its own
        // instances are admitted; the by-name cast below cannot name it and
        // admitted anything. HotSpot's text names the proxy by `getName()`.
        if meta.class_id >= 0x8000_0000 {
            let proxy = ClassId::new(meta.class_id);
            if crate::lang_class::lambda_proxy_class_name(&*ctx, proxy).is_some() {
                return Some(owner_cast_failure(ctx, receiver, proxy));
            }
        }
        let target = format!("L{};", meta.class_name);
        let recv = Value::Object(Some(receiver));
        if reference_arg_admitted(ctx, recv, &target) == Some(false) {
            return Some(reference_cast_failure(ctx, recv, &target));
        }
    }
    vh_value_refusal(ctx, meta, value?)
}

/// `CRATONVM_VH_CAS_EXPECTED_CAST` (nio21 wave 12, lane L3; default ON, both
/// modes): the field and static arms of `compareAndSet` / `weakCompareAndSet*`
/// / `compareAndExchange*` cast the EXPECTED value to the variable's type, as
/// the JDK's bodies do (`handle.fieldType.cast(expected)`, evaluated before
/// the new value's cast), so a wrong-typed expectation is HotSpot's
/// `ClassCastException` instead of a failed comparison. `0` restores
/// judging the new value alone
/// (`docs/feature-designs/native-dispatch-architecture.md`).
pub(crate) fn vh_cas_expected_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_CAS_EXPECTED_CAST")
    })
}

/// The refusal an INSTANCE-field compare-and-set / compare-and-exchange owes,
/// in the JDK's argument order: the receiver (`receiverType.cast(holder)`),
/// then the expectation (`fieldType.cast(expected)`, under
/// [`vh_cas_expected_cast_enabled`]), then the new value
/// (`fieldType.cast(value)`). Judged on the same terms as
/// [`vh_instance_refusal`]: nothing for a handle with no meta or no class name.
pub(crate) fn vh_instance_cas_refusal(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    receiver: ObjectRef,
    expected: Value,
    new_val: Value,
) -> Option<MethodCallFailed> {
    if !vh_cas_expected_cast_enabled() {
        return vh_instance_refusal(ctx, meta, receiver, Some(new_val));
    }
    if let Some(refusal) = vh_instance_refusal(ctx, meta, receiver, Some(expected)) {
        return Some(refusal);
    }
    let meta = meta.filter(|m| !m.class_name.is_empty())?;
    vh_value_refusal(ctx, meta, new_val)
}

/// The STATIC-field twin of [`vh_instance_cas_refusal`]: the expectation's
/// cast (under [`vh_cas_expected_cast_enabled`]), then the new value's.
pub(crate) fn vh_static_cas_refusal(
    ctx: &mut dyn NativeContext,
    meta: &VarHandleMeta,
    expected: Value,
    new_val: Value,
) -> Option<MethodCallFailed> {
    if vh_cas_expected_cast_enabled() {
        if let Some(refusal) = vh_value_refusal(ctx, meta, expected) {
            return Some(refusal);
        }
    }
    vh_value_refusal(ctx, meta, new_val)
}

/// The VALUE half of [`vh_instance_refusal`]: refuse a value that does not fit
/// the handle's declared field type. Shared with the STATIC-field store paths
/// (`set`, `compareAndSet`, `compareAndExchange`, `getAndSet`), which judged
/// nothing at all until round 11 wave 4 — `VH.set((Object) "x")` on a static
/// `int[]` or `Integer` field stored the `String`.
pub(crate) fn vh_value_refusal(
    ctx: &mut dyn NativeContext,
    meta: &VarHandleMeta,
    value: Value,
) -> Option<MethodCallFailed> {
    // A PRIMITIVE field given a reference. Every store-mode native now runs
    // `vh_adapt_value_args` first, which unboxes a wrapper and throws the
    // call-site-correct CCE / WMTE / NPE for anything else (round 11 wave 16),
    // so a reference reaches this arm only from a caller that skipped the
    // adaptation. It stays as the backstop it was: a non-wrapper is refused
    // (WMTE, the `String`-typed-site answer) rather than stored as an address.
    // Its old claim that a wrapper was fine to STORE here was the wrong value
    // of `R11W15RtVarHandleObjectSite` -- the wrapper's address went into the
    // slot.
    if matches!(
        meta.field_desc.as_str(),
        "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C"
    ) {
        if let Value::Object(Some(o)) = value {
            // An `Integer[]`'s header carries `Integer`'s class id, and read as
            // a wrapper it was STORED into the `int` field. An array is never a
            // wrapper; name it as the array it is.
            let (name, is_array) = if ctx.object_is_array(o) {
                (entry_value_type_display(&*ctx, o), true)
            } else {
                let cid = ctx.class_id_of_object(o);
                (ctx.class_name_of_id(cid).unwrap_or_default(), false)
            };
            if is_array || crate::lang_class::wrapper_to_prim_desc(&name).is_none() {
                // The variable's Java name (`int`), not its descriptor
                // (`I`), as every other refusal here spells it. Built before
                // the throw borrows `ctx` mutably.
                let text = format!(
                    "cannot convert {} to {}",
                    name.class_text(&*ctx),
                    vh_prim_java_name(meta.field_desc.as_bytes()[0])
                );
                return Some(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/invoke/WrongMethodTypeException",
                    &text,
                ));
            }
        }
        return None;
    }
    // An ARRAY-typed field. `reference_arg_admitted` answers "cannot tell" for
    // every `[` descriptor, so `VH.set(h, (Object) "x")` on an `int[]` field
    // used to STORE the `String` (r11w3-rt-varhandle-array-typed-field-stores-unchecked).
    if meta.field_desc.starts_with('[') {
        if let Value::Object(Some(obj)) = value {
            if vh_array_field_value_admitted(&*ctx, obj, &meta.field_desc) == Some(false) {
                let have = entry_value_type_display(&*ctx, obj);
                return Some(
                    RuntimeError::ClassCastException {
                        message: format!(
                            "Cannot cast {have} to {}",
                            jvm_type_display(&meta.field_desc)
                        ),
                    }
                    .into(),
                );
            }
        }
        return None;
    }
    if reference_arg_admitted(ctx, value, &meta.field_desc) == Some(false) {
        return Some(reference_cast_failure(ctx, value, &meta.field_desc));
    }
    None
}

// ---------------------------------------------------------------------------
// VarHandle value adaptation: the `asType` step a primitive variable owes its
// value parameters (round 11 wave 16, lane vh)
// ---------------------------------------------------------------------------
//
// # The wrong value this closes
//
// `F.set(h, (Object) Integer.valueOf(i))` on an `int` field `h.f` read back
// the POINTER BITS of the `Integer`, not `i`, in every mode including
// `--nojit` (probe `R11W15RtVarHandleObjectSite`: `boxed bad=20000`, HotSpot
// `bad=0`). The value arrives as `Value::Object(Some(Integer))` because the
// call site's static type is `Object`; `vh_value_refusal` admitted every
// wrapper, `varhandle_set` handed the reference to `set_field`, and
// `coerce_field_value_for_slot` stored the address into the `int` slot
// (`PointerIntoPrimitive`, 60 000 hits in the run's descriptor-coercion
// census). A `Short` read back the same way, a `Long` (HotSpot: CCE) was
// stored as its address, and a `String` threw WMTE where HotSpot throws CCE.
// `compareAndSet(h, (Object) 0, (Object) 1)` compared an `Integer` reference
// against the `int` slot and always FAILED.
//
// HotSpot runs `asType(callSiteType)` before the access. For a primitive
// variable of type `p` and a value whose call-site static type is `s`:
//
// * `s` primitive: widening only (`primitive_widens(s, p)`), else WMTE. The
//   widening is NUMERIC: `set(h, 5)` on a `float` field stores `5.0f`. The
//   heap's slot coercion reinterprets an `Int` at an `F` slot as raw bits,
//   so the conversion has to happen here.
// * `s` a reference: `MethodType.canConvert` admits it statically only if
//   `s` is a supertype of `p`'s wrapper, or a wrapper that widens to `p`
//   (`String`, `Long`-into-`int`: WMTE). At run time `unboxWiden` then
//   takes a null to NPE, a non-wrapper to CCE (`(Number) x`), a wrapper that
//   does not widen to `p` to CCE (`Integer.class.cast(x)`), and widens the
//   rest numerically.
//
// `s` comes from `cratonvm_native_api::poly_call_site::take_vh_value_site`.
// When no door armed it (the JIT's site-cached dispatch, a direct call), the
// static check is skipped and the run-time `Object`-site rules apply: a
// wrapper is unboxed and widened, anything else is CCE, null is NPE.

/// The Java name of a primitive descriptor byte.
pub(crate) fn vh_prim_java_name(p: u8) -> &'static str {
    match p {
        b'Z' => NAME_BOOLEAN,
        b'B' => NAME_BYTE,
        b'C' => NAME_CHAR,
        b'S' => NAME_SHORT,
        b'I' => NAME_INT,
        b'J' => NAME_LONG,
        b'F' => NAME_FLOAT,
        b'D' => NAME_DOUBLE,
        _ => "?",
    }
}

/// The dotted wrapper class name of a primitive descriptor byte, as
/// `Class.cast`'s message spells it.
pub(crate) fn vh_prim_wrapper_display(p: u8) -> &'static str {
    match p {
        b'Z' => "java.lang.Boolean",
        b'B' => "java.lang.Byte",
        b'C' => "java.lang.Character",
        b'S' => "java.lang.Short",
        b'I' => "java.lang.Integer",
        b'J' => "java.lang.Long",
        b'F' => "java.lang.Float",
        b'D' => "java.lang.Double",
        _ => "java.lang.Object",
    }
}

/// `MethodType.canConvert(site, p)` for a value parameter whose call-site
/// class is `site` and a PRIMITIVE variable `p`. `None` when the site is
/// unknown (nothing armed), which skips the static half.
pub(crate) fn vh_site_admits_primitive(site: u8, p: u8) -> Option<bool> {
    use cratonvm_native_api::poly_call_site::vh_site;
    if site == vh_site::UNKNOWN {
        return None;
    }
    if vh_site::is_primitive(site) {
        return Some(primitive_widens(site, p));
    }
    if let Some(w) = vh_site::wrapper_prim(site) {
        return Some(primitive_widens(w, p));
    }
    Some(match site {
        // Every wrapper is `Serializable`, `Comparable` and (JDK 15+)
        // `Constable`.
        vh_site::OBJECT | vh_site::SERIALIZABLE | vh_site::COMPARABLE | vh_site::CONSTABLE => true,
        // `Boolean` and `Character` are not `Number`s.
        vh_site::NUMBER => !matches!(p, b'Z' | b'C'),
        // Only `Integer`, `Long`, `Float` and `Double` are `ConstantDesc`s.
        vh_site::CONSTANT_DESC => matches!(p, b'I' | b'J' | b'F' | b'D'),
        _ => false,
    })
}

/// Widen `value`, read as primitive kind `src`, to primitive kind `dst`
/// numerically (JLS 5.1.2). `None` when `value` does not read as `src` or
/// `dst` is not reachable from it; the caller then keeps the value as it is.
///
/// `raw_slot` is set for a value read out of a wrapper's `value` field, where
/// an `F` slot can present as its raw bits in a `Value::Int`. An ARGUMENT is
/// never raw: an `Int` there is an `int`.
pub(crate) fn vh_widen_primitive(value: Value, src: u8, dst: u8, raw_slot: bool) -> Option<Value> {
    enum Num {
        I(i32),
        J(i64),
        F(f32),
        D(f64),
    }
    let n = match (src, value) {
        (b'Z' | b'B' | b'C' | b'S' | b'I', Value::Int(i)) => Num::I(i),
        (b'Z' | b'B' | b'C' | b'S' | b'I', Value::Long(l)) => Num::I(l as i32), // Cast: compact slot, low half
        (b'J', Value::Long(l)) => Num::J(l),
        (b'J', Value::Int(i)) => Num::J(i as i64),
        (b'F', Value::Float(f)) => Num::F(f),
        (b'F', Value::Int(bits)) if raw_slot => Num::F(f32::from_bits(bits as u32)), // Cast: raw float bits
        (b'D', Value::Double(d)) => Num::D(d),
        _ => return None,
    };
    Some(match (dst, n) {
        (b'Z' | b'B' | b'C' | b'S' | b'I', Num::I(i)) => Value::Int(i),
        (b'J', Num::I(i)) => Value::Long(i as i64),
        (b'J', Num::J(l)) => Value::Long(l),
        (b'F', Num::I(i)) => Value::Float(i as f32),
        (b'F', Num::J(l)) => Value::Float(l as f32),
        (b'F', Num::F(f)) => Value::Float(f),
        (b'D', Num::I(i)) => Value::Double(i as f64),
        (b'D', Num::J(l)) => Value::Double(l as f64),
        (b'D', Num::F(f)) => Value::Double(f as f64),
        (b'D', Num::D(d)) => Value::Double(d),
        _ => return None,
    })
}

/// The `WrongMethodTypeException` for a value parameter `asType` refuses
/// statically.
pub(crate) fn vh_value_wmte(ctx: &mut dyn NativeContext, value: Value, p: u8) -> MethodCallFailed {
    let from = match value {
        Value::Object(Some(o)) if ctx.object_is_array(o) => entry_value_type_display(&*ctx, o),
        Value::Object(Some(o)) => {
            let cid = ctx.class_id_of_object(o);
            ctx.class_name_of_id(cid)
                .unwrap_or_default()
                .class_text(&*ctx)
        }
        Value::Object(None) => "null".to_string(),
        Value::Int(_) => NAME_INT.to_string(),
        Value::Long(_) => NAME_LONG.to_string(),
        Value::Float(_) => NAME_FLOAT.to_string(),
        Value::Double(_) => NAME_DOUBLE.to_string(),
        _ => "?".to_string(),
    };
    crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        &format!("cannot convert {from} to {}", vh_prim_java_name(p)),
    )
}

/// Raise the `WrongMethodTypeException` of an `asType` refusal of a VarHandle
/// parameter (`raise`, which keeps today's two-type text) and leave the
/// handle's shape for the door, which knows the call site's descriptor and
/// rewrites the message as HotSpot's
/// `cannot convert MethodHandle(<access mode type>) to <call-site type>`
/// ([`varhandle_astype_refusal_with_site`]; interpreter round i2 wave 53,
/// lane L4,
/// `interpreter-L4-varhandle-null-coordinate-and-value-type-messages-differ-FIXED-20261017`).
/// `vh` must be current: the shape is read before `raise` allocates. A handle
/// that cannot describe itself exactly leaves no record (the text stays).
#[cold]
#[inline(never)]
pub(crate) fn vh_astype_refused(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    raise: impl FnOnce(&mut dyn NativeContext) -> MethodCallFailed,
) -> MethodCallFailed {
    // An `IndirectVarHandle` describes itself through its own fields; every
    // other handle through this model's row (or a real array handle's class).
    let shape = if is_indirect_var_handle(ctx, vh) {
        indirect_vh_shape(ctx, vh)
    } else if let Some(shape) = segment_vh_shape(ctx, vh) {
        // A real `SegmentVarHandle` (wave 57, lane L4): its form's types.
        Some(shape)
    } else {
        let (_kind, value, coordinates, exact) = vh_value_and_coordinate_descriptors(ctx, vh);
        exact.then_some((coordinates, value))
    };
    let failed = raise(ctx);
    if let Some((coordinates, value)) = shape {
        if let MethodCallFailed::ExceptionThrown(exception) = &failed {
            cratonvm_native_api::poly_call_site::note_vh_astype_refusal(
                cratonvm_native_api::poly_call_site::VhAsTypeRefusal {
                    exception: *exception,
                    coordinates,
                    value,
                },
            );
        }
    }
    failed
}

/// `asType`'s RETURN direction for a VarHandle access mode that returns its
/// variable (`get*`, `getAndSet*`, `getAndAdd*`, `getAndBitwise*`,
/// `compareAndExchange*`): at a call site whose return the door left in the
/// parameter-site word's top byte (`poly_call_site::vh_param_site_word`), a
/// variable whose type does not convert to it (`MethodType.canConvert`,
/// [`mh_can_convert`]: a narrowing primitive, a reference that is neither a
/// wrapper supertype nor a wrapper widening to it, a primitive whose wrapper
/// the reference site is not a supertype of) is HotSpot's
/// `WrongMethodTypeException`, raised before the access, its text rewritten
/// by the door as for a parameter ([`vh_astype_refused`]). A primitive site
/// answered `0` before (the box of the variable's own type reached the
/// site's unboxing, which zeroes a foreign wrapper); a reference site raised
/// W6-1's own text (`vm_exec::unbox_poly_return_checked`).
///
/// Hot path: one predictable branch for an `Object` or `void` site (`ret ==
/// 0`), and otherwise byte compares against the row the native's prologue
/// already read (the variable's descriptor). Everything else -- a mismatch,
/// a handle with no row -- is the cold half.
///
/// Interpreter round i2 wave 57, lane L4
/// (`interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021`,
/// `CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE`).
///
/// Answers the site's primitive when the variable is typed as exactly that
/// primitive's wrapper (`Integer` at an `int` site), else `0`: `asType`
/// unboxes such a variable through `ValueConversions.unbox<W>(<W> x)`, whose
/// `null` raises `Cannot invoke "<W>.<p>Value()" because "x" is null`, where
/// every other reference variable goes through `primitiveConversion`
/// ([`vh_own_wrapper_null`]; interpreter round i2 wave 59, lane L4).
#[inline]
pub(crate) fn vh_return_site_check(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    site: u64,
    row: &VhPrologueRow,
) -> Result<u8, MethodCallFailed> {
    let ret = cratonvm_native_api::poly_call_site::vh_param_site_return(site);
    if ret == 0 {
        return Ok(0);
    }
    if let Some(Some(m)) = row {
        match m.field_desc.as_bytes().first() {
            // The site's own primitive.
            Some(&v) if v == ret => return Ok(0),
            // A reference variable at a reference site (a class code sits
            // below every descriptor letter): `R -> R` always converts, the
            // cast is the access's (VH2-3).
            Some(b'L' | b'[') if ret < b'A' => return Ok(0),
            // A primitive at its own wrapper's site (`(Integer) ints.get(h)`).
            Some(&v)
                if cratonvm_native_api::poly_call_site::vh_site::wrapper_prim(ret) == Some(v) =>
            {
                return Ok(0)
            }
            _ => {}
        }
    }
    vh_return_site_check_cold(ctx, args, ret, row)
}

/// The access's result `result` at a primitive site `own` whose variable is
/// typed as that primitive's wrapper ([`vh_return_site_check`]'s answer; `0`:
/// not such a site): a `null` is HotSpot's `unbox<W>(<W> x)` NPE, which
/// names `"x"` (`L4W58VarHandleReturnNullText` row `integer-null-as-int`,
/// measured on 25.0.3). Everything else passes through. One predictable
/// branch on the hot path. Interpreter round i2 wave 59, lane L4.
#[inline]
pub(crate) fn vh_own_wrapper_null(
    ctx: &mut dyn NativeContext,
    own: u8,
    result: MethodCallResult,
) -> MethodCallResult {
    if own == 0 {
        return result;
    }
    vh_own_wrapper_null_cold(ctx, own, result)
}

#[cold]
#[inline(never)]
pub(crate) fn vh_own_wrapper_null_cold(
    ctx: &mut dyn NativeContext,
    own: u8,
    result: MethodCallResult,
) -> MethodCallResult {
    if !matches!(result, Ok(Some(Value::Object(None)))) {
        return result;
    }
    let prim = byte_view_desc(own);
    let Some(wrapper) = wrapper_desc_for_primitive(prim) else {
        return result;
    };
    match mh_unbox_for_primitive(ctx, prim, Value::Object(None), Some(wrapper)) {
        Err(npe) => Err(npe),
        Ok(_) => result,
    }
}

/// The cold half of [`vh_return_site_check`]: the variable's descriptor from
/// the handle's FFM layout carrier, its row, or a real array handle's
/// component (a handle that describes itself no other way is not judged),
/// against the site's primitive `ret`.
#[cold]
#[inline(never)]
pub(crate) fn vh_return_site_check_cold(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ret: u8,
    row: &VhPrologueRow,
) -> Result<u8, MethodCallFailed> {
    if !crate::nbflags().vh_primitive_return_astype {
        return Ok(0);
    }
    let Some(Value::Object(Some(vh))) = args.first() else {
        return Ok(0);
    };
    let vh = *vh;
    let variable = if let Some(shape) = p67_segment_vh_shape(&*ctx, vh) {
        Some(layout_vh_carrier_desc(shape.carrier).to_string())
    } else if let Some(m) = vh_row_or_read(ctx, vh, row) {
        Some(m.field_desc.clone())
    } else if is_indirect_var_handle(ctx, vh) {
        // A combinator (`MethodHandles.filterValue`, ...): its own `value`.
        indirect_vh_field(ctx, vh, "value").and_then(|m| strict_mirror_descriptor(&*ctx, m))
    } else if let Some((_, value)) = segment_vh_shape(ctx, vh) {
        // A real `SegmentVarHandle` (`ValueLayout.varHandle()` under
        // `--jdk-only`): its form's erased get type.
        Some(value)
    } else {
        real_array_var_handle_descriptors(&*ctx, vh).map(|(value, _)| value)
    };
    let Some(variable) = variable.filter(|v| !v.is_empty()) else {
        return Ok(0);
    };
    // The variable typed as the site's own wrapper (wave 59, lane L4): it
    // converts, and its `null` names `"x"` (`vh_own_wrapper_null`).
    if ret >= b'A'
        && byte_view_desc(ret).as_bytes()[0] == ret
        && wrapper_desc_for_primitive(byte_view_desc(ret)) == Some(variable.as_str())
    {
        return Ok(ret);
    }
    let converts = if ret < b'A' {
        // A reference site other than `Object`: only a PRIMITIVE variable
        // is judged (`P -> R` must box to a subtype of the site's class).
        let v = variable.as_bytes()[0];
        !vh_site_is_primitive_desc(v) || vh_site_class_accepts_primitive(ret, v)
    } else {
        let ret_desc = byte_view_desc(ret);
        ret_desc.as_bytes()[0] != ret || mh_can_convert(&variable, ret_desc)
    };
    if converts {
        return Ok(0);
    }
    // `raise`'s own text names the two types, as the parameter refusals do;
    // the door that armed the word rewrites it as HotSpot's
    // `cannot convert MethodHandle(<access mode type>) to <call-site type>`.
    let site_name = if ret < b'A' {
        "the call site's type"
    } else {
        vh_prim_java_name(ret)
    };
    let text = format!(
        "cannot convert {} to {site_name}",
        descriptor_to_class_name(&variable).class_text(&*ctx),
    );
    Err(vh_astype_refused(ctx, vh, |ctx| {
        crate::phases_early::throw_jca_exc(ctx, "java/lang/invoke/WrongMethodTypeException", &text)
    }))
}

/// Is `b` the descriptor byte of one of the eight primitives?
pub(crate) fn vh_site_is_primitive_desc(b: u8) -> bool {
    cratonvm_native_api::poly_call_site::vh_site::is_primitive(b)
}

/// `MethodType.canConvert(p, site)` for a PRIMITIVE variable `p` returned
/// into a reference call site of [`vh_site`](cratonvm_native_api::poly_call_site::vh_site)
/// class `class` (never `OBJECT`, which the word does not carry): the site's
/// class must be assignable from `p`'s wrapper ([`reference_accepts_wrapper`]'s
/// closed table, by class code).
pub(crate) fn vh_site_class_accepts_primitive(class: u8, p: u8) -> bool {
    use cratonvm_native_api::poly_call_site::vh_site;
    match class {
        vh_site::OBJECT | vh_site::SERIALIZABLE | vh_site::COMPARABLE | vh_site::CONSTABLE => true,
        vh_site::NUMBER => matches!(p, b'B' | b'S' | b'I' | b'J' | b'F' | b'D'),
        vh_site::CONSTANT_DESC => matches!(p, b'I' | b'J' | b'F' | b'D'),
        // A wrapper site takes exactly its own primitive's box; any other
        // class (and every array type) takes none.
        _ => vh_site::wrapper_prim(class) == Some(p),
    }
}

/// The door half of [`vh_astype_refused`]: when `failed` is the exception a
/// VarHandle native refused with and left a record for, rewrite its message
/// with the access mode type of `method_name` and the call site's
/// `call_site` descriptor, each with the leading `VarHandle` the invoker's
/// type carries, as `MethodType.toString` prints them (simple names). Every
/// other failure, and every way of not knowing, returns `failed` unchanged.
///
/// For the doors that armed the parameter-site word and hold the real
/// call-site descriptor: the interpreter's signature-polymorphic block, its
/// site-cache replay, and the JIT's site-cached native dispatch. Allocates
/// (the new text); the exception is pinned across it.
#[cold]
#[inline(never)]
pub fn varhandle_astype_refusal_with_site(
    ctx: &mut dyn NativeContext,
    failed: MethodCallFailed,
    method_name: &str,
    call_site: &str,
) -> MethodCallFailed {
    let MethodCallFailed::ExceptionThrown(exception) = &failed else {
        return failed;
    };
    let exception = *exception;
    let Some(record) = cratonvm_native_api::poly_call_site::take_vh_astype_refusal(exception)
    else {
        return failed;
    };
    let Some(access_type) = vh_access_type_of_method(method_name) else {
        return failed;
    };
    let own = vh_access_mode_descriptor(access_type, &record.coordinates, &record.value);
    let shown = vh_invoker_descriptor(&own)
        .and_then(|d| method_type_display_in(&*ctx, &d))
        .zip(vh_invoker_descriptor(call_site).and_then(|d| method_type_display_in(&*ctx, &d)));
    let Some((own, site)) = shown else {
        return failed;
    };
    let text = format!("cannot convert MethodHandle{own} to {site}");
    let pin = ctx.pin_native_root(exception);
    let detail = ctx.create_string_uninterned(&text);
    let exception = ctx.read_native_pin(pin, exception);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(exception, "detailMessage", Value::Object(Some(detail)));
    MethodCallFailed::ExceptionThrown(exception)
}

/// ONE value parameter of a VarHandle access on a PRIMITIVE variable of type
/// `p`, adapted as `asType` would: see the block comment above.
pub(crate) fn vh_adapt_value(
    ctx: &mut dyn NativeContext,
    p: u8,
    value: Value,
    site: u8,
) -> Result<Value, MethodCallFailed> {
    use cratonvm_native_api::poly_call_site::vh_site;
    match value {
        Value::Object(maybe) => {
            if vh_site_admits_primitive(site, p) == Some(false) {
                return Err(vh_value_wmte(ctx, value, p));
            }
            let Some(o) = maybe else {
                return Err(RuntimeError::NullPointerException {
                    message: Some(format!(
                        "Cannot unbox a null value into a VarHandle variable of type {}",
                        vh_prim_java_name(p)
                    )),
                }
                .into());
            };
            if ctx.object_is_array(o) {
                return Err(RuntimeError::ClassCastException {
                    message: format!(
                        "Cannot cast {} to java.lang.Number",
                        entry_value_type_display(&*ctx, o)
                    ),
                }
                .into());
            }
            let cid = ctx.class_id_of_object(o);
            let name = ctx.class_name_of_id(cid).unwrap_or_default();
            let Some(wdesc) = crate::lang_class::wrapper_to_prim_desc(&name) else {
                return Err(RuntimeError::ClassCastException {
                    message: format!("Cannot cast {} to java.lang.Number", name.class_text(&*ctx)),
                }
                .into());
            };
            let w = wdesc.as_bytes()[0];
            if !primitive_widens(w, p) {
                return Err(RuntimeError::ClassCastException {
                    message: format!(
                        "Cannot cast {} to {}",
                        name.class_text(&*ctx),
                        vh_prim_wrapper_display(p)
                    ),
                }
                .into());
            }
            let raw = crate::lang_class::coerce_reflective_field_value(ctx.get_field(o, 0), wdesc);
            // A wrapper whose payload does not read as its own kind is not
            // one this can convert; keep the reference and let the store
            // arms below report it, which is today's behaviour for it.
            Ok(vh_widen_primitive(raw, w, p, true).unwrap_or(value))
        }
        Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
            let src = if vh_site::is_primitive(site) {
                if !primitive_widens(site, p) {
                    return Err(vh_value_wmte(ctx, value, p));
                }
                site
            } else {
                match value {
                    Value::Int(_) => b'I',
                    Value::Long(_) => b'J',
                    Value::Float(_) => b'F',
                    _ => b'D',
                }
            };
            Ok(vh_widen_primitive(value, src, p, false).unwrap_or(value))
        }
        other => Ok(other),
    }
}

/// The primitive type of the variable a VarHandle access touches, or `None`
/// for a reference variable and for every handle shape this does not adapt
/// (byte-array/`ByteBuffer` views, FFM layout and segment handles).
pub(crate) fn vh_primitive_var_desc(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    args: &[Value],
) -> Option<u8> {
    match meta {
        Some(m) if m.kind == VH_KIND_INSTANCE || m.kind == VH_KIND_STATIC => {
            primitive_char(&m.field_desc)
        }
        // A byte-array or `ByteBuffer` view (round 15 wave 3, lane vh4): its
        // variable is the view's primitive (`field_desc`), and `asType`
        // converts the VALUE to it like any other. Before, the raw value went
        // to `byte_view_encode`/`byte_view_set`, which encode its BITS: an
        // `int` into a `float` view stored `Float.intBitsToFloat(5)` for `5`,
        // a boxed `Integer` through an `Object` site stored `0`, and a `long`
        // into an `int` view stored its low half where HotSpot throws
        // `WrongMethodTypeException`.
        Some(m)
            if matches!(
                m.kind,
                VH_KIND_BYTE_VIEW_LE
                    | VH_KIND_BYTE_VIEW_BE
                    | VH_KIND_BYTE_BUFFER_VIEW_LE
                    | VH_KIND_BYTE_BUFFER_VIEW_BE
            ) =>
        {
            if vh_view_value_adapt_enabled() {
                primitive_char(&m.field_desc)
            } else {
                None
            }
        }
        Some(m) if m.kind != VH_KIND_ARRAY => None,
        // An array-element handle: our ARRAY kind, or a real-JDK
        // `VarHandle*$Array` with no side-table entry. The shape test is
        // `vh_array_call`'s, minus the meta read already made here.
        _ => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return None,
            };
            if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array)
                || !matches!(args.get(2), Some(Value::Int(_)))
            {
                return None;
            }
            primitive_char(array_element_desc(&*ctx, arr))
        }
    }
}

/// Adapt the trailing `n_values` value parameters of a VarHandle store-mode
/// call (`set*`: 1, `getAndSet*`/`getAndAdd*`/`getAndBitwise*`: 1,
/// `compareAnd*`/`weakCompareAndSet*`: 2) for a PRIMITIVE variable, using the
/// call-site classes `site` the door armed (`take_vh_value_site`, low byte =
/// last parameter, high byte = the one before it).
///
/// Borrows `args` unchanged — no allocation — for a reference variable, for a
/// shape it does not adapt, and whenever every value already has the
/// variable's own kind, which is every exactly-typed call site.
pub(crate) fn vh_adapt_value_args<'a>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &'a [Value],
    n_values: usize,
    site: u16,
) -> Result<Cow<'a, [Value]>, MethodCallFailed> {
    vh_adapt_value_args_row(ctx, this, args, n_values, site, &None)
}

/// The variable type [`vh_check_value_site_row`] judges the values against
/// (JIT round 21, lane vhmerge;
/// `docs/internal/fixed-bugs/jr21-vhmerge-varhandle-array-regression-from-dev-merge-FIXED-20261026.md`).
///
/// That check runs BEFORE the array coordinate's cast, so for an
/// array-element handle the variable type must be the HANDLE's declared
/// component type (`asType` judges the method type, which knows nothing of
/// the run-time array), not the element type of whatever array the caller
/// passed. [`vh_primitive_var_desc`] reads the run-time array: correct after
/// the cast (the two agree whenever the cast admits), wrong before it, where
/// `VZ.getAndBitwiseOr((Object) ints, 0, true)` judged `true` against `int`
/// and threw `WrongMethodTypeException` instead of HotSpot's
/// `ClassCastException` (`R20FuzzGVhArrayAll1` refuse code 62 -> 88,
/// `R20FuzzIVhArr1` 22222 CCE -> WMTE). The declared type comes from the
/// same place the cast reads it (the side-table `class_name`, or the real-JDK
/// handle's first coordinate); when it is unknown, or the cast is switched
/// off, the run-time answer stands as before.
pub(crate) fn vh_static_value_var_desc(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    args: &[Value],
) -> Option<u8> {
    let array_arm = match meta {
        Some(m) => m.kind == VH_KIND_ARRAY,
        // A real-JDK handle has no row; an FFM one's second coordinate is a
        // `long` offset (the cast's own early-out).
        None => !matches!(
            args.get(2),
            Some(Value::Long(_) | Value::Float(_) | Value::Double(_))
        ),
    };
    if array_arm && vh_array_coordinate_cast_enabled() {
        let real: Option<Arc<str>>;
        let declared: Option<&str> = match meta {
            Some(m) => Some(m.class_name.as_str()),
            None => {
                real = vh_real_array_coordinate_desc(&*ctx, this);
                real.as_deref()
            }
        };
        if let Some(d) = declared.and_then(|d| d.strip_prefix('[')) {
            // `[I` -> `I`; a reference or nested array component is not a
            // primitive variable, so no primitive value check applies.
            return primitive_char(d);
        }
    }
    vh_primitive_var_desc(ctx, meta, args)
}

/// Check static value conversions before executing coordinate casts or null
/// checks. This does not unbox values: runtime conversion still follows the
/// coordinate adapter, while an impossible method type wins before either.
pub(crate) fn vh_check_value_site_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    n_values: usize,
    site: u16,
    row: &VhPrologueRow,
) -> Result<(), MethodCallFailed> {
    if n_values == 0 || args.len() < 1 + n_values {
        return Ok(());
    }
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(());
    };
    let meta = vh_row_or_read(ctx, this, row);
    let Some(p) = vh_static_value_var_desc(ctx, this, meta.as_deref(), args) else {
        return Ok(());
    };
    let first = args.len() - n_values;
    let site_of = |i: usize| -> u8 {
        match args.len() - 1 - i {
            0 => (site & 0xff) as u8,
            1 => (site >> 8) as u8,
            _ => 0,
        }
    };
    // `asType` runs `MethodType.canConvert` over EVERY parameter before it
    // converts any value, so a static refusal of the second value wins over a
    // run-time refusal of the first: `compareAndSet(h, (Object) null,
    // (String) s)` on an `int` field is WMTE, not the first value's NPE.
    // `vh_adapt_value` repeats each check; this pass only fixes the ORDER.
    for (i, v) in args.iter().enumerate().skip(first) {
        let s = site_of(i);
        let refused = match v {
            Value::Object(_) => vh_site_admits_primitive(s, p) == Some(false),
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
                cratonvm_native_api::poly_call_site::vh_site::is_primitive(s)
                    && !primitive_widens(s, p)
            }
            _ => false,
        };
        if refused {
            let v = *v;
            return Err(vh_astype_refused(ctx, this, |ctx| vh_value_wmte(ctx, v, p)));
        }
    }
    Ok(())
}

/// [`vh_adapt_value_args`] over the row the prologue read (wave 47,
/// [`VARHANDLE_ONE_ROW_READ`]).
pub(crate) fn vh_adapt_value_args_row<'a>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &'a [Value],
    n_values: usize,
    site: u16,
    row: &VhPrologueRow,
) -> Result<Cow<'a, [Value]>, MethodCallFailed> {
    if n_values == 0 || args.len() < 1 + n_values {
        return Ok(Cow::Borrowed(args));
    }
    // "Already the variable's own kind" needs the variable's type, so the
    // type is asked first. `vh_meta_get` is a thread-local memo hit on the
    // hot path, and a reference variable returns right after it.
    let meta = vh_row_or_read(ctx, this, row);
    let Some(p) = vh_primitive_var_desc(ctx, meta.as_deref(), args) else {
        // A REFERENCE variable given a primitive value boxes it
        // (`vh_box_value_args`); everything else is borrowed unchanged.
        return vh_box_value_args(ctx, this, meta.as_deref(), args, n_values, site);
    };
    let first = args.len() - n_values;
    let natural = |v: &Value| match p {
        b'J' => matches!(v, Value::Long(_)),
        b'F' => matches!(v, Value::Float(_)),
        b'D' => matches!(v, Value::Double(_)),
        _ => matches!(v, Value::Int(_)),
    };
    // A primitive site that NARROWS (`set(h, 5)` on a `byte` field) arrives
    // in the natural `Int` representation, so a known site is judged even
    // then; an unknown one cannot be.
    let site_of = |i: usize| -> u8 {
        let from_end = args.len() - 1 - i;
        match from_end {
            0 => (site & 0xff) as u8, // Cast: low byte = last parameter
            1 => (site >> 8) as u8,   // Cast: high byte = the one before it
            _ => 0,
        }
    };
    let needs = (first..args.len()).any(|i| {
        !natural(&args[i]) || {
            let s = site_of(i);
            cratonvm_native_api::poly_call_site::vh_site::is_primitive(s) && s != p
        }
    });
    if !needs {
        return Ok(Cow::Borrowed(args));
    }
    vh_check_value_site_row(ctx, args, n_values, site, row)?;
    let mut out = args.to_vec();
    for (i, slot) in out.iter_mut().enumerate().skip(first) {
        *slot = vh_adapt_value(ctx, p, *slot, site_of(i))?;
    }
    Ok(Cow::Owned(out))
}

// ---------------------------------------------------------------------------
// VarHandle value adaptation, REFERENCE variable: `asType` BOXES a primitive
// value (round 11 wave 17, lane vh)
// ---------------------------------------------------------------------------
//
// javac does not autobox the argument of a signature-polymorphic call, so
// `O.set(h, 5)` on an `Object` field is `invokevirtual VarHandle.set:(LH;I)V`
// and the native receives `Value::Int(5)`. HotSpot's `asType` boxes it
// (`Integer.valueOf(5)`) when `MethodType.canConvert(int, R)` holds -- when
// the variable's declared type `R` is a supertype of the wrapper -- and throws
// `WrongMethodTypeException` otherwise (`int` into a `String` or `Long`
// field). CratonVM stored the raw `Int` and `coerce_field_value_for_slot`
// turned it into `null` at the reference slot
// (r11w16-vh-primitive-into-reference-variable-stores-null).
//
// Boxing ALLOCATES, and a moving collection there would leave every reference
// in `args` -- the handle, the receiver or array, a reference expected value --
// stale in the copy handed back. So every reference argument and every box is
// rooted in a `NativeHandleScope` and the returned vector is rebuilt from the
// handles after the LAST allocation. The callers re-read the handle from the
// returned vector for the same reason.

/// The primitive descriptor string of a descriptor byte, for the boxing call.
pub(crate) fn vh_prim_desc_str(p: u8) -> &'static str {
    match p {
        b'Z' => "Z",
        b'B' => "B",
        b'C' => "C",
        b'S' => "S",
        b'J' => "J",
        b'F' => "F",
        b'D' => "D",
        _ => "I",
    }
}

/// `MethodType.canConvert(p, R)` for a PRIMITIVE value `p` and a REFERENCE
/// variable declared `decl` (`L..;` or `[..`): `R.isAssignableFrom(wrapper(p))`.
///
/// Every wrapper is an `Object`, a `Serializable`, a `Comparable` and (JDK 15+)
/// a `Constable`; all but `Boolean` and `Character` are `Number`s; only
/// `Integer`, `Long`, `Float` and `Double` are `ConstantDesc`s. The same table
/// as [`vh_site_admits_primitive`], read from the other side.
pub(crate) fn vh_reference_admits_primitive(decl: &str, p: u8) -> bool {
    if decl.starts_with('[') {
        return false;
    }
    let name = decl
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(decl);
    match name {
        "java/lang/Object"
        | "java/io/Serializable"
        | "java/lang/Comparable"
        | "java/lang/constant/Constable" => true,
        "java/lang/Number" => !matches!(p, b'Z' | b'C'),
        "java/lang/constant/ConstantDesc" => matches!(p, b'I' | b'J' | b'F' | b'D'),
        other => crate::lang_class::wrapper_to_prim_desc(other)
            .is_some_and(|w| w.as_bytes().first() == Some(&p)),
    }
}

/// The declared type of the REFERENCE variable a VarHandle access touches.
///
/// `Some(Some(desc))`: a reference variable declared `desc`. `Some(None)`: an
/// array-element handle over a reference array whose handle carries no
/// readable `componentType` (the static check is then skipped, and the store
/// refusals judge the box as they judge any reference). `None`: a primitive
/// variable, or a shape this does not adapt (views, FFM handles, a handle with
/// no side-table entry that is not an array-element access).
pub(crate) fn vh_reference_var_desc(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    this: ObjectRef,
    args: &[Value],
) -> Option<Option<String>> {
    match meta {
        Some(m) if m.kind == VH_KIND_INSTANCE || m.kind == VH_KIND_STATIC => {
            match m.field_desc.as_bytes().first() {
                Some(b'L' | b'[') => Some(Some(m.field_desc.clone())),
                _ => None,
            }
        }
        Some(m) if m.kind != VH_KIND_ARRAY => None,
        // The array-element shape test of `vh_primitive_var_desc`.
        _ => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return None,
            };
            if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array)
                || !matches!(args.get(2), Some(Value::Int(_)))
                || ctx.heap_element_type_of(arr) != ArrayElementType::Reference
            {
                return None;
            }
            // The HANDLE's component type decides `asType`, not the array's
            // run-time one (an `Object[]` handle over a `String[]` boxes, and
            // the store then fails with `ArrayStoreException`).
            let declared = match ctx.get_field_by_name(this, "componentType") {
                Value::Object(Some(c)) => mirror_class_name(&*ctx, c).map(|n| {
                    if n.starts_with('[') {
                        n
                    } else {
                        format!("L{n};")
                    }
                }),
                _ => None,
            };
            Some(declared)
        }
    }
}

/// Adapt the trailing `n_values` value parameters of a VarHandle store-mode
/// call for a REFERENCE variable: a primitive value is boxed as `asType`
/// boxes it, after the static `canConvert` check. See the block comment above.
///
/// Borrows `args` unchanged -- no allocation -- when no value is primitive,
/// which is every reference call site, and for every shape that is not a
/// reference variable.
pub(crate) fn vh_box_value_args<'a>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    args: &'a [Value],
    n_values: usize,
    site: u16,
) -> Result<Cow<'a, [Value]>, MethodCallFailed> {
    use cratonvm_native_api::poly_call_site::vh_site;
    let first = args.len() - n_values;
    let is_prim = |v: &Value| {
        matches!(
            v,
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)
        )
    };
    if !args[first..].iter().any(is_prim) {
        return Ok(Cow::Borrowed(args));
    }
    let Some(declared) = vh_reference_var_desc(ctx, meta, this, args) else {
        return Ok(Cow::Borrowed(args));
    };
    // The primitive TYPE of each value: the armed site's, else the `Value`
    // variant's (`Int` is `int`, right for every `int` site and wrong only for
    // an unarmed `char`/`boolean`/`byte`/`short` one), and the value in that
    // type's own representation.
    let site_of = |i: usize| -> u8 {
        match args.len() - 1 - i {
            0 => (site & 0xff) as u8, // Cast: low byte = last parameter
            1 => (site >> 8) as u8,   // Cast: high byte = the one before it
            _ => 0,
        }
    };
    let typed = |i: usize, v: Value| -> (u8, Value) {
        let from = match v {
            Value::Long(_) => b'J',
            Value::Float(_) => b'F',
            Value::Double(_) => b'D',
            _ => b'I',
        };
        let s = site_of(i);
        if vh_site::is_primitive(s) {
            (s, vh_widen_primitive(v, from, s, false).unwrap_or(v))
        } else {
            (from, v)
        }
    };
    // Static pass first, over every value: `asType` refuses before it boxes.
    if let Some(decl) = declared.as_deref() {
        for (i, v) in args.iter().enumerate().skip(first) {
            if !is_prim(v) {
                continue;
            }
            let (p, _) = typed(i, *v);
            if !vh_reference_admits_primitive(decl, p) {
                let text = format!(
                    "cannot convert {} to {}",
                    vh_prim_java_name(p),
                    jvm_type_display(decl)
                );
                return Err(vh_astype_refused(ctx, this, |ctx| {
                    crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/lang/invoke/WrongMethodTypeException",
                        &text,
                    )
                }));
            }
        }
    }
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let mut rooted: Vec<Option<cratonvm_native_api::NativeHandle>> = Vec::with_capacity(args.len());
    for v in args {
        rooted.push(match v {
            Value::Object(Some(o)) => Some(scope.root(*o)),
            _ => None,
        });
    }
    let mut boxes: Vec<(usize, cratonvm_native_api::NativeHandle)> = Vec::with_capacity(n_values);
    for (i, v) in args.iter().enumerate().skip(first) {
        if !is_prim(v) {
            continue;
        }
        let (p, typed_value) = typed(i, *v);
        // `valueOf`, so the canonical caches apply, as they do to `asType`'s
        // boxing on HotSpot.
        let boxed = box_value_canonical(&mut *scope, typed_value, vh_prim_desc_str(p));
        if let Value::Object(Some(b)) = boxed {
            let h = scope.root(b);
            boxes.push((i, h));
        }
    }
    // After the last allocation: every reference re-read through its handle.
    let mut out = args.to_vec();
    for (i, h) in rooted.iter().enumerate() {
        if let Some(h) = h {
            out[i] = Value::Object(Some(scope.get(h)));
        }
    }
    for (i, h) in &boxes {
        out[*i] = Value::Object(Some(scope.get(h)));
    }
    Ok(Cow::Owned(out))
}

/// The element type a ONE-dimensional primitive array descriptor (`[I`, `[B`,
/// ...) names, or `None` for every other descriptor — including `[[I`, whose
/// elements are references.
pub(crate) fn one_dim_primitive_array_element(desc: &str) -> Option<ArrayElementType> {
    let b = desc.as_bytes();
    if b.len() != 2 || b[0] != b'[' {
        return None;
    }
    Some(match b[1] {
        b'Z' => ArrayElementType::Boolean,
        b'C' => ArrayElementType::Char,
        b'F' => ArrayElementType::Float,
        b'D' => ArrayElementType::Double,
        b'B' => ArrayElementType::Byte,
        b'S' => ArrayElementType::Short,
        b'I' => ArrayElementType::Int,
        b'J' => ArrayElementType::Long,
        _ => return None,
    })
}

/// May the non-null `value` be stored into a field declared with the ARRAY
/// descriptor `field_desc`? Same contract as [`reference_arg_admitted`]:
/// `Some(false)` is a positive mismatch, `None` is "cannot tell, do not refuse".
///
/// Array-ness comes from the heap, not the class name — a CratonVM array's
/// `class_id_of_object` is its COMPONENT class (see `entry_value_casts_to`,
/// which settles the same shape question the same way). What the shape
/// settles on its own:
///
/// * a non-array is never an array;
/// * a one-dimensional primitive array field admits exactly that array;
/// * a primitive array is never a reference array (`Object[]`, `String[]`) nor
///   a multi-dimensional one (`int[]` is not an `int[][]`).
///
/// A reference array into a reference-array field (`Integer[]` into a
/// `String[]` field) needs its component class, so the VM judges it with
/// `checkcast`'s own predicate (`NativeClassAccess::array_value_casts_to_desc`,
/// the interpreter's `array_is_assignable_to`) — a `VarHandle` set is
/// `asType` plus a cast to the field type, so it refuses exactly what
/// `checkcast` refuses (an `Object[]` is no `T[]` in any mode since
/// i10-L2). A context that cannot answer (`None`, the mocks) still fails open
/// (r11w7-rt-varhandle-ref-array-field-check-patch).
pub(crate) fn vh_array_field_value_admitted(
    ctx: &dyn NativeContext,
    value: ObjectRef,
    field_desc: &str,
) -> Option<bool> {
    vh_array_field_value_admitted_with(ctx, value, field_desc, |v, d| {
        ctx.array_value_casts_to_desc(v, d)
    })
}

/// [`vh_array_field_value_admitted`] with the reference-array verdict passed
/// in, so a test can drive the `Some(false)` arm without a VM behind the
/// context. Production passes `NativeClassAccess::array_value_casts_to_desc`.
pub(crate) fn vh_array_field_value_admitted_with(
    ctx: &dyn NativeContext,
    value: ObjectRef,
    field_desc: &str,
    reference_array_casts: impl FnOnce(ObjectRef, &str) -> Option<bool>,
) -> Option<bool> {
    if !field_desc.starts_with('[') {
        return None;
    }
    if !ctx.object_is_array(value) {
        return Some(false);
    }
    let have = ctx.heap_element_type_of(value);
    match one_dim_primitive_array_element(field_desc) {
        Some(want) => Some(have == want),
        None if have == ArrayElementType::Reference => reference_array_casts(value, field_desc),
        None => Some(false),
    }
}

/// The exception an array-element `VarHandle` store of `value` into `arr`
/// raises, or `None` when the store may proceed
/// (r11w4-rt-varhandle-array-element-stores-unchecked).
///
/// The JDK's `VarHandleReferences.Array` makes two checks, and this VM never
/// ran either, so an `Integer` could be stored into a `String[]`:
///
/// 1. the invocation's `asType` casts the value to the handle's own
///    `componentType`, which gives a `ClassCastException`;
/// 2. `runtimeTypeCheck` checks the value against the ARRAY's actual
///    component (a `String[]` seen through an `Object[]` handle), which gives
///    an `ArrayStoreException`.
///
/// Both refuse only on a positive answer. Check 1 goes through
/// [`reference_arg_admitted`]. Check 2 goes through
/// `aastore_element_assignable`, the predicate the interpreter's `aastore`
/// uses, which must never produce a false `ArrayStoreException`. A primitive
/// array, a null value, and a handle with no readable `componentType` (our
/// synthetic layout) skip the check that does not apply.
pub(crate) fn vh_array_store_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    value: Value,
) -> Option<MethodCallFailed> {
    vh_array_store_refusal_route(ctx, vh, arr, value, false)
}

/// [`vh_array_store_refusal`] with the store's road: `aastore` is plain
/// `set` (an `aastore` in the JDK body, so a class-name message), `false`
/// every `runtimeTypeCheck` mode ([`vh_array_store_check_failure`]).
pub(crate) fn vh_array_store_refusal_route(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    value: Value,
    aastore: bool,
) -> Option<MethodCallFailed> {
    let Value::Object(Some(stored)) = value else {
        return None;
    };
    if ctx.heap_element_type_of(arr) != ArrayElementType::Reference {
        return None;
    }
    if let Some(refusal) = vh_array_component_cast_refusal(ctx, vh, stored) {
        // Wave 26 (lane vhase): a `runtimeTypeCheck` mode on an array that is
        // not exactly the handle's `arrayType` takes the JDK's slow path,
        // `reflectiveTypeCheck` against the array's REAL component. That
        // component is a subtype of the handle's (the coordinate cast held),
        // so a value the handle's component refuses is refused there too:
        // `new ArrayStoreException()`, not the fast path's `Class.cast` CCE.
        if !aastore && vh_array_covariant_slow_path(ctx, vh, arr) {
            return Some(vh_array_store_check_failure(&*ctx, stored, false));
        }
        return Some(refusal);
    }
    if ctx.aastore_element_assignable(arr, stored) == Some(false) {
        return Some(vh_array_store_check_failure(&*ctx, stored, aastore));
    }
    None
}

/// `CRATONVM_VH_ARRAY_HIDDEN_COMPONENT_CAST` (default ON, both modes; JIT
/// round 21 wave 26, lane vhase): a HIDDEN `componentType` cannot be resolved
/// by name, so `reference_arg_admitted` answered `None` (admit) and plain
/// `set` of a `String` through `arrayElementVarHandle(H[].class)` fell to the
/// `aastore` check (`ArrayStoreException: java.lang.String`). HotSpot's
/// `componentType.cast(value)` throws `ClassCastException: Cannot cast
/// java.lang.String to H` first (`R21Fz10VhCovariant1`, `HV/covH set
/// String`). The verdict is taken on the mirror's class id instead: a hidden
/// class is never named by another class's `extends` / `implements`, so its
/// only instances are its own and those of subclasses `is_subclass` knows.
/// `0` restores the admit.
pub(crate) fn vh_array_hidden_component_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_ARRAY_HIDDEN_COMPONENT_CAST")
    })
}

/// Whether `value` is an instance of the HIDDEN class the mirror `component`
/// reflects: `Some(verdict)`, or `None` when the switch is off, the mirror
/// names no class id, or that class is not hidden (the name path decides).
pub(crate) fn vh_hidden_component_admits(
    ctx: &dyn NativeContext,
    component: ObjectRef,
    value: ObjectRef,
) -> Option<bool> {
    if !vh_array_hidden_component_cast_enabled() {
        return None;
    }
    let cid = mirror_class_id(ctx, component)?;
    if !ctx.is_class_hidden(cid) {
        return None;
    }
    if ctx.object_is_array(value) {
        return Some(false);
    }
    let vid = ctx.class_id_of_object(value);
    Some(vid == cid || ctx.is_subclass(vid, cid))
}

/// `CRATONVM_VH_ARRAY_COVARIANT_SLOW_PATH` (default ON, both modes; JIT round
/// 21 wave 26, lane vhase): JDK 25 `VarHandleReferences.Array.runtimeTypeCheck`
/// casts the value to the handle's `componentType` (a `ClassCastException`)
/// only when `handle.arrayType == oarray.getClass()`; otherwise it runs
/// `reflectiveTypeCheck`, whose failure is a message-less
/// `ArrayStoreException`. HotSpot 25.0.3 measured (`R21Fz10VhCovariant1`,
/// `CS/covCS {setVolatile,getAndSet,CAS} Integer`: an `Integer` through a
/// `CharSequence[]` handle into a `String[]`): `ArrayStoreException()`, where
/// CratonVM threw `Cannot cast java.lang.Integer to java.lang.CharSequence`.
/// Plain `set` always casts (`componentType.cast(value)`), so it is not
/// switched. `0` restores the CCE on every array.
pub(crate) fn vh_array_covariant_slow_path_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_ARRAY_COVARIANT_SLOW_PATH")
    })
}

/// Whether `arr` is positively NOT of the handle's exact `arrayType`
/// (`runtimeTypeCheck`'s slow path), under
/// [`vh_array_covariant_slow_path_enabled`]. Decided on the heap header (a
/// reference array's header carries its COMPONENT's class id) against the
/// handle's `componentType` mirror. Answers `false` (the fast path, the old
/// behaviour) whenever it cannot be sure: no readable mirror or class id, a
/// primitive component, or a header id of `0`. An array component is
/// compared by name ([`vh_array_name_is_covariant`]; round 22 wave 1).
pub(crate) fn vh_array_covariant_slow_path(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
) -> bool {
    if !vh_array_covariant_slow_path_enabled() {
        return false;
    }
    let Value::Object(Some(component)) = ctx.get_field_by_name(vh, "componentType") else {
        return false;
    };
    let Some(name) = mirror_class_name(&*ctx, component) else {
        return false;
    };
    if matches!(
        name.as_str(),
        "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short"
    ) {
        return false;
    }
    if name.starts_with('[') {
        // JIT round 22 wave 1, lane review (same switch): an ARRAY component
        // (an `Object[][]` handle's `Object[]`) is decided by name, since the
        // header's id is then the component ARRAY class's. A `String[][]`
        // through an `Object[][]` handle takes `reflectiveTypeCheck` too, so
        // an `Integer` stored by `setVolatile` / `getAndSet` / CAS is the
        // message-less `ArrayStoreException`, not check 1's `Cannot cast
        // java.lang.Integer to [Ljava.lang.Object;`. Different names prove
        // different classes; equal names keep the fast path, as before.
        if ctx.class_id_of_object(arr).as_u32() == 0
            || ctx.heap_element_type_of(arr) != ArrayElementType::Reference
        {
            return false;
        }
        let have = crate::lang_class::array_class_internal_name(&*ctx, arr);
        return vh_array_name_is_covariant(&have, &name);
    }
    let Some(want) = mirror_class_id(&*ctx, component) else {
        return false;
    };
    let have = ctx.class_id_of_object(arr);
    vh_array_header_is_covariant(have.as_u32(), want.as_u32())
}

/// [`vh_array_covariant_slow_path`]'s verdict on raw ids: a known (non-zero)
/// header component that differs from the handle's component.
pub(crate) fn vh_array_header_is_covariant(header_component: u32, handle_component: u32) -> bool {
    header_component != 0 && header_component != handle_component
}

/// [`vh_array_covariant_slow_path`]'s verdict for an ARRAY component (JIT
/// round 22 wave 1, lane review): the array's own class name `array_name`
/// (`[[Ljava/lang/String;`) is not the handle's `arrayType`, `[` + the
/// component's name. Either spelling of the separators is accepted.
pub(crate) fn vh_array_name_is_covariant(array_name: &str, handle_component: &str) -> bool {
    let have = array_name.replace('.', "/");
    let want = format!("[{}", handle_component.replace('.', "/"));
    have != want
}

/// `CRATONVM_VH_ARRAY_RUNTIME_TYPE_CHECK_NULL_MESSAGE` (default ON, both
/// modes; JIT round 21 wave 25, lane vhase): the `ArrayStoreException` of a
/// reference-array `VarHandle` store refused by the array's OWN component
/// carries HotSpot's message. JDK 25 `VarHandleReferences.Array` has two
/// roads: plain `set` is `array[index] = handle.componentType.cast(value)`,
/// an `aastore`, whose refusal names the value's class (`java.lang.Integer`);
/// every other store mode (`setVolatile` / `setOpaque` / `setRelease`, the
/// `compareAndSet` / `compareAndExchange*` / `weakCompareAndSet*` family and
/// `getAndSet*`) passes the value through `runtimeTypeCheck` ->
/// `reflectiveTypeCheck`, which catches the `Class.cast` failure and throws
/// `new ArrayStoreException()`: a NULL message (measured, HotSpot 25.0.3,
/// `R21VhConstRefRows` cold rows). `0` restores the class name on every mode.
pub(crate) fn vh_array_runtime_type_check_null_message_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_VH_ARRAY_RUNTIME_TYPE_CHECK_NULL_MESSAGE",
        )
    })
}

/// The `ArrayStoreException` a reference-array `VarHandle` store of `stored`
/// owes once the array's own component refused it: `aastore` (plain `set`)
/// names the value's class; the `runtimeTypeCheck` modes have no message
/// under [`vh_array_runtime_type_check_null_message_enabled`].
pub(crate) fn vh_array_store_check_failure(
    ctx: &dyn NativeContext,
    stored: ObjectRef,
    aastore: bool,
) -> MethodCallFailed {
    vh_array_store_check_error(
        aastore,
        vh_array_runtime_type_check_null_message_enabled(),
        || entry_value_type_display(ctx, stored),
    )
    .into()
}

/// [`vh_array_store_check_failure`]'s decision, the switch passed in. An
/// EMPTY `ArrayStoreException` message is a NULL `getMessage()`
/// (`RuntimeError::as_java_throwable`).
pub(crate) fn vh_array_store_check_error(
    aastore: bool,
    null_message: bool,
    class_text: impl FnOnce() -> String,
) -> RuntimeError {
    let message = if aastore || !null_message {
        class_text()
    } else {
        String::new()
    };
    RuntimeError::ArrayStoreException { message }
}

/// The `compareAndSet` / `compareAndExchange` EXPECTED value's half of
/// [`vh_array_store_refusal`] (round 15 wave 4, lane vh5): `asType` casts it
/// to the handle's `componentType` too (`VarHandleReferences.Array` also
/// casts it in the body), so a wrong-typed expectation is
/// `ClassCastException` on HotSpot, where CratonVM compared it and answered
/// `false` / the witness. Check 1 only: `runtimeTypeCheck` is the stored
/// value's. Before check 1 of the NEW value, in parameter order. Under
/// `CRATONVM_VH_ARRAY_COORDINATE_CAST`.
pub(crate) fn vh_array_expected_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    expected: Value,
) -> Option<MethodCallFailed> {
    let Value::Object(Some(expected)) = expected else {
        return None;
    };
    if !vh_array_coordinate_cast_enabled()
        || ctx.heap_element_type_of(arr) != ArrayElementType::Reference
    {
        return None;
    }
    vh_array_component_cast_refusal(ctx, vh, expected)
}

/// `CRATONVM_VH_ARRAY_CAST_BEFORE_INDEX` (default on, both modes; round 15
/// wave 5, lane vh7; F3 of `r15w4-vh5-review-findings-CLOSED-20260930.md`): the
/// reference array `set` arm runs the stored value's cast to the handle's
/// `componentType` (check 1 of [`vh_array_store_refusal`]) BEFORE its index
/// check, and only the `ArrayStoreException` check after it. HotSpot 25
/// measured (`R15Vh7ArrayCastOrder`): `set` casts first (`setOob` CCE), while
/// `compareAndSet`, `compareAndExchange` and `getAndSet` check the index
/// first (AIOOBE), so those arms keep index-first and are not switched. `0`
/// restores index-first for `set` too (`SA.set((Object) strs, 99, (Object)
/// 1)`: `ArrayIndexOutOfBoundsException` instead of HotSpot's
/// `ClassCastException`).
pub(crate) fn vh_array_cast_before_index_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_ARRAY_CAST_BEFORE_INDEX")
    })
}

/// `CRATONVM_VH_ARRAY_CAST_BEFORE_INDEX_SET_ONLY` (default ON, both modes;
/// JIT round 21 wave 26, lane vhase): [`vh_array_cast_before_index_enabled`]
/// applies to plain `set` ONLY. `varhandle_set_mode` serves all four set
/// modes, so the cast-first order also reached `setVolatile` / `setRelease` /
/// `setOpaque`, whose JDK 25 bodies (`VarHandleReferences.Array`) are
/// `UNSAFE.putReference*(array, (long) Preconditions.checkIndex(..) <<
/// ashift + abase, runtimeTypeCheck(handle, array, value))`: Java evaluates
/// the offset argument (the index check) BEFORE the value's cast. Plain `set`
/// is `array[index] = handle.componentType.cast(value)`, whose RHS runs
/// before the `aastore`'s own bounds check, so it alone casts first.
/// HotSpot 25.0.3 measured (`R21Fz9VhArrRef1`, `wrong@-1` rows): `set` CCE,
/// `setVolatile` / `setRelease` / `setOpaque` AIOOBE. `0` restores cast-first
/// on all four set modes.
pub(crate) fn vh_array_cast_before_index_set_only_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_VH_ARRAY_CAST_BEFORE_INDEX_SET_ONLY",
        )
    })
}

/// Whether the set arm serving `mode` casts the value BEFORE its index check.
pub(crate) fn vh_array_mode_casts_before_index(mode: &str) -> bool {
    vh_array_mode_casts_before_index_with(
        mode,
        vh_array_cast_before_index_enabled(),
        vh_array_cast_before_index_set_only_enabled(),
    )
}

/// [`vh_array_mode_casts_before_index`] with both switches passed in.
pub(crate) fn vh_array_mode_casts_before_index_with(mode: &str, cast_first: bool, set_only: bool) -> bool {
    cast_first && (mode == "set" || !set_only)
}

/// The refusal the reference array `set` arm owes BEFORE its index check
/// under [`vh_array_mode_casts_before_index`]: the stored value's cast to
/// the handle's `componentType`. `None` with the switch off, for a mode that
/// checks the index first, for a primitive array, and for values the cast
/// admits.
pub(crate) fn vh_array_casts_before_index(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    stored: Value,
    mode: &str,
) -> Option<MethodCallFailed> {
    if !vh_array_mode_casts_before_index(mode) {
        return None;
    }
    let Value::Object(Some(stored)) = stored else {
        return None;
    };
    if ctx.heap_element_type_of(arr) != ArrayElementType::Reference {
        return None;
    }
    vh_array_component_cast_refusal(ctx, vh, stored)
}

/// What the reference array `set` arm checks AFTER its index check: all of
/// [`vh_array_store_refusal`], or, when [`vh_array_casts_before_index`]
/// already ran the cast, only check 2 (`ArrayStoreException`: for plain `set`
/// worded with the stored value's class name, as HotSpot's `aastore`:
/// `java.lang.Integer`; for `setVolatile` / `setOpaque` / `setRelease` no
/// message, [`vh_array_store_check_failure`]).
pub(crate) fn vh_array_refusal_after_index(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    stored: Value,
    mode: &str,
) -> Option<MethodCallFailed> {
    // Wave 25 (lane vhase): only plain `set` is an `aastore` in the JDK body;
    // `setVolatile` / `setOpaque` / `setRelease` go through `runtimeTypeCheck`
    // (a null-message `ArrayStoreException`).
    let aastore = mode == "set";
    // Wave 26 (lane vhase): a mode that did not cast before the index runs
    // the whole refusal (cast, then the array's own component) here.
    if !vh_array_mode_casts_before_index(mode) {
        return vh_array_store_refusal_route(ctx, vh, arr, stored, aastore);
    }
    let Value::Object(Some(stored)) = stored else {
        return None;
    };
    if ctx.heap_element_type_of(arr) != ArrayElementType::Reference {
        return None;
    }
    if ctx.aastore_element_assignable(arr, stored) == Some(false) {
        return Some(vh_array_store_check_failure(&*ctx, stored, aastore));
    }
    None
}

/// Check 1 of [`vh_array_store_refusal`]: `asType`'s cast of the non-null
/// reference `value` to a real-JDK array handle's `componentType`, or `None`.
pub(crate) fn vh_array_component_cast_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    stored: ObjectRef,
) -> Option<MethodCallFailed> {
    let value = Value::Object(Some(stored));
    if let Value::Object(Some(component)) = ctx.get_field_by_name(vh, "componentType") {
        if let Some(name) = mirror_class_name(&*ctx, component) {
            // `reference_arg_admitted` judges `L...;` only; an array or
            // primitive component name is left to check 2.
            let primitive = matches!(
                name.as_str(),
                "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short"
            );
            if !primitive && !name.starts_with('[') {
                let target = format!("L{name};");
                // Wave 26 (lane vhase): an undecided name (a hidden component
                // does not resolve by name) is asked of the mirror's class id.
                let verdict = reference_arg_admitted(ctx, value, &target);
                if verdict == Some(false)
                    || (verdict.is_none()
                        && vh_hidden_component_admits(&*ctx, component, stored) == Some(false))
                {
                    return Some(reference_cast_failure(ctx, value, &target));
                }
            }
            // Round 15 wave 4 (lane vh5): an ARRAY component (an `int[][]`
            // handle's `int[]`) is `asType`'s cast of the value as well, so a
            // `long[]` stored through it is `ClassCastException: Cannot cast
            // [J to [I` on HotSpot, not the `ArrayStoreException` check 2
            // raised. Under the coordinate cast's switch (the same `asType`).
            if name.starts_with('[')
                && vh_array_coordinate_cast_enabled()
                && vh_array_coordinate_admits(&*ctx, stored, &name) == Some(false)
            {
                return Some(reference_cast_failure(ctx, value, &name));
            }
        }
    }
    None
}

/// `CRATONVM_MH_INVOKE_RETURN_REFUSAL` -- default ON (JIT round 16, lane mh; a
/// correctness fix, both modes): the `invoke` door refuses a call site whose
/// return the handle's return cannot convert to (`asType`'s
/// `WrongMethodTypeException`). `0`/`false`/`off`/`no` restores the
/// parameter-only check. Read once.
pub(crate) fn mh_invoke_return_refusal_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_RETURN_REFUSAL")
    })
}

/// Do the `MethodHandles` combinator factories run the JDK's argument checks
/// (`dropArgumentChecks`, `filterArgumentsCheckArity` / `filterArgumentChecks`,
/// `filterReturnValue`'s, `foldArgumentChecks`, `collectArguments`',
/// `catchException`'s, `permuteArguments`', `guardWithTest`'s) in this VM's
/// mode? `--jdk-only` always (the round-15 rule); `--compatible` too since JIT
/// round 16 (lane mh), because both modes target HotSpot: `R16MhErrors`
/// `drop-pos` / `filter-mismatch` / `filter-ret-mismatch` built adapters under
/// `--compatible` where HotSpot 25 throws `IllegalArgumentException`.
/// `CRATONVM_MH_COMBINATOR_CHECKS_COMPATIBLE=0` restores the `--jdk-only`-only
/// gate (a kill switch for a `--compatible` arm that trips on a synthetic
/// handle's type). Read only under `--compatible`.
///
/// Interpreter round i1 wave 50 (lane L4) landed the same `--compatible` arm
/// per VM: [`NativeContext::mh_combinator_checks`] (`ClassRealm`,
/// `CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE`, on by default). Merged: the
/// checks run when that per-VM answer says so, and under `--compatible` either
/// variable at `0` turns the `--compatible` arm off.
pub(crate) fn mh_combinator_checks_in_this_mode(ctx: &dyn NativeContext) -> bool {
    static COMPAT_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // `JdkInvoke` (nio21 wave 12, L3): the row takes `--jdk-only`'s arm.
    ctx.mh_combinator_checks()
        && (ctx.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke)
            || *COMPAT_ON.get_or_init(|| {
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_MH_COMBINATOR_CHECKS_COMPATIBLE",
                )
            }))
}

/// `CRATONVM_MH_INVOKE_COLLECTOR_ARITY_REFUSAL` -- default ON (JIT round 16,
/// lane mh; a correctness fix, both modes): a variable-arity collector's
/// `invoke` at another arity judges each collected argument's conversion to
/// the array's component ([`invoke_collector_arity_refusal`]), an array at the
/// site's tail is the fixed-arity pass-through rather than an element, and a
/// refusal at the collected position names the collected type, as HotSpot's
/// `asCollector(..).asType(site)` does. `0`/`false`/`off`/`no` restores the
/// round-15 rule. Read once.
pub(crate) fn mh_invoke_collector_arity_refusal_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_COLLECTOR_ARITY_REFUSAL")
    })
}

/// `(p0 .. p(n-2), component x k)ret`: the type of `asCollector(arrayType, k)`
/// over a handle whose parameters are `params` (the last one the array).
pub(crate) fn collected_descriptor(params: &[String], ret: &str, component: &str, k: usize) -> String {
    let fixed = &params[..params.len().saturating_sub(1)];
    let mut out = String::from("(");
    for p in fixed {
        out.push_str(p);
    }
    for _ in 0..k {
        out.push_str(component);
    }
    out.push(')');
    out.push_str(ret);
    out
}

/// [`invoke_narrowing_arg_refusal`] for a call site whose arity is not the
/// handle's (round 16, lane mh): only a variable-arity collector called with
/// at least its fixed parameters has an answer, `asCollector(arrayType, k)
/// .asType(site)`. Every fixed position, every collected argument (against the
/// component) and the return are `MethodType.canConvert` ([`mh_can_convert`]);
/// a refusal is `WrongMethodTypeException: cannot convert MethodHandle<the
/// collected type> to <site>`. HotSpot 25, `R16MhErrors` `invoke-varargs-bad`:
/// `vsum(int...)` invoked as `(int,String)int` is `cannot convert
/// MethodHandle(int,int)int to (int,String)int`; this door collected the
/// `String` as `0` and answered 1. `None` for every other shape.
pub(crate) fn invoke_collector_arity_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    declared_params: &[String],
    declared_ret: &str,
    site_params: &[String],
    site_ret: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    if !mh_invoke_collector_arity_refusal_enabled() || !mh_is_varargs_collector(&*ctx, mh) {
        return None;
    }
    let n = declared_params.len();
    let component = declared_params.last()?.strip_prefix('[')?;
    if site_params.len() + 1 < n {
        return None;
    }
    let fixed = n - 1;
    let k = site_params.len() - fixed;
    let params_refused = site_params.iter().enumerate().any(|(i, site)| {
        let target = if i < fixed {
            declared_params[i].as_str()
        } else {
            component
        };
        !mh_can_convert(site, target)
    });
    let return_refused =
        !mh_can_convert(declared_ret, site_ret) && mh_invoke_return_refusal_enabled();
    if !params_refused && !return_refused {
        return None;
    }
    let collected = collected_descriptor(declared_params, declared_ret, component, k);
    let shown_declared = method_type_display_in(&*ctx, &collected)?;
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
    ))
}

pub(crate) fn invoke_narrowing_arg_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    call_site: &str,
    declared: Option<&str>,
) -> Option<MethodCallFailed> {
    if !mh_strict_exact_signature()
        || !(kind_has_authoritative_adapted_type(kind) || invoker_has_own_type(&*ctx, mh, kind))
    {
        return None;
    }
    // The handle's `mh_declared_descriptor`, read once by the door (round 12
    // wave 7).
    let declared = declared?;
    if declared == call_site || (declared == "()V" && call_site != "()V") {
        return None;
    }
    let (declared_params, declared_ret) = split_descriptor_params(&declared)?;
    let (site_params, site_ret) = split_descriptor_params(call_site)?;
    if declared_params.len() != site_params.len() {
        // Round 16 (lane mh): a variable-arity collector called at ANOTHER
        // arity is `asCollector(arrayType, k).asType(site)`, which converts
        // every collected argument to the component.
        return invoke_collector_arity_refusal(
            ctx,
            mh,
            &declared_params,
            &declared_ret,
            &site_params,
            &site_ret,
            call_site,
        );
    }
    // Round 16 (lane mh): `asType` converts the handle's RETURN to the call
    // site's as well (`MethodType.canConvert(newType.rtype, rtype)`, in the
    // adapter's direction: the handle's return to the site's), so
    // `(String) intToInt.invoke(3)` and `(short) intToInt.invoke(3)` are
    // `WrongMethodTypeException` on HotSpot (`R16MhErrors` `invoke-ret-str` /
    // `invoke-ret-short`). This door answered 6, and for the `String` site
    // handed an `Integer` to code typed `String`. A refused return or a
    // refused parameter below raises the same exception with the same
    // message, so the order between them does not show.
    let return_refused =
        !mh_can_convert(&declared_ret, &site_ret) && mh_invoke_return_refusal_enabled();
    // Round 15 wave 5 (lane mh4): a pair with a REFERENCE on one side and a
    // primitive on the other is `MethodType.canConvert`'s too (`mh_can_convert`,
    // the transcription `asType` already uses): a `Long` at an `int` parameter
    // unboxes to `long`, which does not narrow, so `asType` refuses it with
    // `WrongMethodTypeException` before any argument is looked at. This door
    // let it through to the argument casts, whose unboxing raised
    // `ClassCastException` (`R15Mh2InvokeExactType` `null-order row3`: HotSpot
    // WMTE=5000, CratonVM CCE=5000 in every arm). Two references are a
    // runtime cast, never this refusal's. Switch
    // `CRATONVM_MH_INVOKE_BOXED_ARG_REFUSAL` (default ON), read only on a pair
    // that refuses.
    //
    // A variable-arity COLLECTOR's trailing array parameter is not converted
    // but COLLECTED when the site's argument there is no array of it
    // (`AsVarargsCollector.asTypeUncached`: `asCollector(arrayType, 1)
    // .asType(newType)`), so that position is judged against the array's
    // COMPONENT: `SHOW(Object[]).invoke(7)` collects the `7` (HotSpot
    // "1:Integer"; `R15Mh2InvokeExactType` `sites-rotate` case 5, which this
    // rule refused as `(Object[])String` to `(int)String` until hand-back 3).
    let collector_tail = if mh_is_varargs_collector(&*ctx, mh) {
        declared_params
            .last()
            .and_then(|p| p.strip_prefix('['))
            .map(|component| (declared_params.len() - 1, component.to_string()))
    } else {
        None
    };
    let mut narrows = false;
    let mut refused_at_tail = false;
    for (i, (d, site)) in declared_params.iter().zip(site_params.iter()).enumerate() {
        let refused = match (&collector_tail, primitive_char(d), primitive_char(site)) {
            // The collected position: the element's conversion to the
            // component (`canConvert`; two references are a runtime cast),
            // under this rule's switch, as before the collector rule there
            // was no refusal at an array position at all.
            // Round 16 (lane mh): an ARRAY at the site's tail may be the
            // collector's own array type (`arrayType.isAssignableFrom`), which
            // `AsVarargsCollector.asTypeUncached` passes through at fixed
            // arity; judging it against the component refused
            // `(String,int[])V`'s collector at `(Object,int[])V`. An array is
            // never refused here (fail open: two references are a runtime
            // cast).
            (Some((tail, _)), _, _)
                if *tail == i
                    && site.starts_with('[')
                    && mh_invoke_collector_arity_refusal_enabled() =>
            {
                false
            }
            (Some((tail, component)), _, _) if *tail == i => {
                !mh_can_convert(site, component)
                    && cratonvm_types::flags::runtime_flag_default_on(
                        "CRATONVM_MH_INVOKE_BOXED_ARG_REFUSAL",
                    )
            }
            (_, Some(to), Some(from)) => !primitive_widens(from, to),
            (_, None, None) => false,
            _ => {
                !mh_can_convert(site, d)
                    && cratonvm_types::flags::runtime_flag_default_on(
                        "CRATONVM_MH_INVOKE_BOXED_ARG_REFUSAL",
                    )
            }
        };
        if refused {
            narrows = true;
            refused_at_tail = collector_tail.as_ref().is_some_and(|(tail, _)| *tail == i);
            break;
        }
    }
    if !narrows && !return_refused {
        return None;
    }
    // A refusal at a collector's collected position is `asCollector(arrayType,
    // 1).asType(site)`'s, so HotSpot names the COLLECTED type: `cannot convert
    // MethodHandle(int)void to (long)void` for an `(int[])void` collector.
    let shown_declared = match &collector_tail {
        Some((_, component)) if refused_at_tail && mh_invoke_collector_arity_refusal_enabled() => {
            let collected = collected_descriptor(&declared_params, &declared_ret, component, 1);
            method_type_display_in(&*ctx, &collected)?
        }
        _ => method_type_display_in(&*ctx, &declared)?,
    };
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
    ))
}

/// Widen `value` from the return type dispatch produced to the return type the
/// handle DECLARES, or `None` when no widening applies.
///
/// **Why a retyped handle needs this at all.** `asType` here is a STAMP:
/// `mh_with_stamped_type` clones the handle and writes the new `MethodType`
/// into `type`, leaving `MH_DESC` -- the descriptor dispatch actually runs
/// against -- as the leaf member's. So
/// `findStatic(Math,"max",(int,int)int).asType((int,int)long)` ran the leaf,
/// produced an `int`, and boxed it as `Integer` against the LEAF descriptor.
/// The call site wanted `J`, so the return coercion downstream fabricated a
/// zero -- and, once the strict return-type check landed, threw instead -- on a
/// call HotSpot answers `2`. Recorded as the known blind spot in
/// `vm_exec::mh_strict_invokeexact`'s own comment; this is the half that closes
/// it.
///
/// Widening only, and only between primitives, exactly the set JLS 5.1.2
/// permits. `byte`/`short`/`char`/`int` share `Value::Int` here, so the
/// integral-to-integral widenings need no conversion and are absent below.
/// Anything else -- a narrowing, a reference, a `void` -- returns `None` and
/// leaves today's behaviour untouched.
pub(crate) fn widen_return_value(value: Value, from_ret: u8, to_ret: u8) -> Option<Value> {
    if from_ret == to_ret {
        return None;
    }
    let as_i64 = |v: Value| match v {
        Value::Int(i) => Some(i64::from(i)),
        Value::Long(l) => Some(l),
        _ => None,
    };
    Some(match (from_ret, to_ret) {
        (b'B' | b'S' | b'C' | b'I', b'J') => Value::Long(as_i64(value)?),
        (b'B' | b'S' | b'C' | b'I', b'F') => Value::Float(as_i64(value)? as f32),
        (b'B' | b'S' | b'C' | b'I', b'D') => Value::Double(as_i64(value)? as f64),
        (b'J', b'F') => Value::Float(as_i64(value)? as f32),
        (b'J', b'D') => Value::Double(as_i64(value)? as f64),
        (b'F', b'D') => match value {
            Value::Float(f) => Value::Double(f64::from(f)),
            _ => return None,
        },
        _ => return None,
    })
}

/// Narrow `value` from the return type dispatch produced to the return type
/// the handle DECLARES — the JLS 5.1.3 primitive narrowing conversions,
/// which is the piece [`widen_return_value`] deliberately excludes.
///
/// **Why this is safe beside a widening-only sibling.** A stamped `type`
/// NARROWER than `MH_DESC`'s return is only ever reachable through
/// `explicitCastArguments`: `asType` refuses a narrowing conversion eagerly,
/// at `mh_astype_refusal` (before any handle carrying one is ever produced),
/// so a handle that reaches `box_return_against_target` with a narrowing
/// return cannot have come from `asType`. `explicitCastArguments`'s own
/// contract is exactly JLS 5.1.3-style narrowing (an inserted cast), not
/// `asType`'s widening-only rule — see `mh_explicit_cast_refusal`'s doc
/// comment. Measured (`x.explicitCastReturn`,
/// `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`):
/// `explicitCastArguments(len, (String)byte)` on a 6-char string answered
/// `0` — the un-narrowed `int` boxed against the LEAF descriptor, which the
/// call site's own unboxing (expecting a `Byte`) then read as a different
/// wrapper's layout — where HotSpot 25 answers `6`.
///
/// Uses Rust's `as` casts throughout: for the integral truncations that
/// matches the JVM's `i2b`/`i2s`/`i2c`/`l2i` bytecode exactly, and for the
/// float/double-to-integral conversions (`f2i`/`f2l`/`d2i`/`d2l`) Rust's
/// saturating-cast semantics (NaN → 0, out-of-range → the bound) match the
/// JVM's specified `f2i`/`d2i`-family behaviour exactly too — this is not a
/// coincidence, both are defined the same way.
pub(crate) fn narrow_return_value(value: Value, from_ret: u8, to_ret: u8) -> Option<Value> {
    if from_ret == to_ret {
        return None;
    }
    let as_i32 = |v: Value| match v {
        Value::Int(i) => Some(i),
        Value::Long(l) => Some(l as i32),
        Value::Float(f) => Some(f as i32),
        Value::Double(d) => Some(d as i32),
        _ => None,
    };
    Some(match (from_ret, to_ret) {
        (b'I' | b'S' | b'C' | b'J' | b'F' | b'D', b'B') => Value::Int(as_i32(value)? as i8 as i32),
        (b'I' | b'C' | b'J' | b'F' | b'D', b'S') => Value::Int(as_i32(value)? as i16 as i32),
        (b'I' | b'S' | b'B' | b'J' | b'F' | b'D', b'C') => Value::Int(as_i32(value)? as u16 as i32),
        (b'J' | b'F' | b'D', b'I') => Value::Int(as_i32(value)?),
        (b'F', b'J') => match value {
            Value::Float(f) => Value::Long(f as i64),
            _ => return None,
        },
        (b'D', b'J') => match value {
            Value::Double(d) => Value::Long(d as i64),
            _ => return None,
        },
        (b'D', b'F') => match value {
            Value::Double(d) => Value::Float(d as f32),
            _ => return None,
        },
        _ => return None,
    })
}

/// Apply [`widen_return_value`]/[`narrow_return_value`] to a dispatch result
/// and re-box it against the TARGET descriptor rather than the leaf one.
///
/// `target` is the handle's declared type for `invokeExact` and the CALL SITE
/// for `invoke` -- the two places the JDK's own `asType(callSiteType)` step
/// would have performed the conversion. Falls back to today's leaf-descriptor
/// boxing whenever the target is absent, unparseable, or not a widening or
/// narrowing this pair of functions covers.
pub(crate) fn box_return_against_target(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    leaf_desc: &str,
    target: Option<&str>,
) -> MethodCallResult {
    let Some(target) = target else {
        return auto_box_return(ctx, result, leaf_desc);
    };
    let from = return_type_desc(leaf_desc).as_bytes().first().copied();
    let to = return_type_desc(target).as_bytes().first().copied();
    let (Some(from), Some(to)) = (from, to) else {
        return auto_box_return(ctx, result, leaf_desc);
    };
    if from == to {
        return auto_box_return(ctx, result, leaf_desc);
    }
    match result {
        Ok(Some(value)) => {
            match widen_return_value(value, from, to)
                .or_else(|| narrow_return_value(value, from, to))
            {
                Some(converted) => auto_box_return(ctx, Ok(Some(converted)), target),
                None => auto_box_return(ctx, Ok(Some(value)), leaf_desc),
            }
        }
        other => auto_box_return(ctx, other, leaf_desc),
    }
}

/// A door's return conversion, decided before its dispatch: everything the
/// door does with the leaf's result afterwards, as one value (interpreter
/// round i2 wave 59, lane L4; proposal
/// `i58-L4-proposal-one-return-plan-for-every-method-handle-door`). Every
/// door (`invoke`, `invokeExact`, the three `invokeWithArguments` paths and
/// the two converting lanes) builds one with [`DoorReturn::invoke`],
/// [`DoorReturn::exact`] or [`DoorReturn::with_arguments`] and hands its
/// result to [`door_return_apply`], where each used to repeat the four steps
/// (and the `invokeWithArguments` doors had once missed one of them).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DoorReturn<'a> {
    pub(crate) kind: i32,
    /// The leaf's descriptor (`MH_DESC`).
    pub(crate) leaf: &'a str,
    /// The handle's declared `type`.
    pub(crate) declared: Option<&'a str>,
    /// The call site [`return_cast_refusal`] casts to (`None`: the declared
    /// type's cast only).
    pub(crate) cast_site: Option<&'a str>,
    /// An `explicitCastArguments` stamp ([`MH_MARK_EXPLICIT_CAST`]).
    pub(crate) explicit_stamp: bool,
    /// The marking bits [`invoke_return_unbox`] reads.
    pub(crate) marks: i32,
    /// The return [`invoke_return_unbox`] converts to.
    pub(crate) unbox_target: Option<&'a str>,
    /// The return [`box_return_against_target`] widens or narrows to
    /// (`None`: the leaf's own boxing).
    pub(crate) box_target: Option<&'a str>,
}

impl<'a> DoorReturn<'a> {
    /// `invoke`'s: `asType(site)`, so the cast, the unboxing and the boxing
    /// are the call site's, except a reference site over a primitive declared
    /// return the leaf does not share ([`invoke_boxes_declared_return`]).
    pub(crate) fn invoke(
        kind: i32,
        leaf: &'a str,
        declared: Option<&'a str>,
        site: Option<&'a str>,
        marks: i32,
        explicit_stamp: bool,
    ) -> Self {
        let box_target = if invoke_boxes_declared_return(leaf, declared, site) {
            declared
        } else {
            site
        };
        DoorReturn {
            kind,
            leaf,
            declared,
            cast_site: site,
            explicit_stamp,
            marks,
            unbox_target: site,
            box_target,
        }
    }

    /// `invokeExact`'s: the declared type is the call site's.
    pub(crate) fn exact(
        kind: i32,
        leaf: &'a str,
        declared: Option<&'a str>,
        marks: i32,
        explicit_stamp: bool,
    ) -> Self {
        DoorReturn {
            kind,
            leaf,
            declared,
            cast_site: None,
            explicit_stamp,
            marks,
            unbox_target: declared,
            box_target: declared,
        }
    }

    /// `invokeWithArguments`'s: an `Object` site, so a primitive is boxed as
    /// the leaf's unless the declared primitive differs
    /// ([`invoke_with_arguments_boxes_declared_return`], `boxes_declared`).
    pub(crate) fn with_arguments(
        kind: i32,
        leaf: &'a str,
        declared: Option<&'a str>,
        marks: i32,
        explicit_stamp: bool,
        boxes_declared: bool,
    ) -> Self {
        DoorReturn {
            box_target: if boxes_declared { declared } else { None },
            ..DoorReturn::exact(kind, leaf, declared, marks, explicit_stamp)
        }
    }

    // The tail decisions (interpreter round i2 wave 60, lane L4; proposal
    // `i58-L4-proposal-one-return-plan-for-every-method-handle-door`, "Left"):
    // whether the door has work after its dispatch, and so whether it may arm
    // `MH_TAIL`, is a projection of the plan; each door used to recompute it
    // from the same inputs.

    /// An `invoke` plan's [`MH_TAIL`] token: the call site's return
    /// ([`door_tail_token`]) unless the return cast has work after the
    /// dispatch ([`return_cast_may_apply`]), else `0`.
    pub(crate) fn invoke_tail(&self) -> u8 {
        if return_cast_may_apply(self.kind, self.leaf, self.declared, self.cast_site) {
            return 0;
        }
        self.cast_site
            .map_or(0, |site| door_tail_token(return_type_desc(site)))
    }

    /// An `invokeExact` plan's [`MH_TAIL`] token for the call site `site`:
    /// the site's return token when the declared return's agrees (the door
    /// converts the leaf's result to the declared return, the interpreter then
    /// to the site's), `0` when the inner return cast has work after the
    /// dispatch or the two disagree.
    pub(crate) fn exact_tail(&self, site: Option<&str>) -> u8 {
        if return_cast_may_apply(self.kind, self.leaf, self.declared, None) {
            return 0;
        }
        let token = site.map_or(0, |site| door_tail_token(return_type_desc(site)));
        let agrees = self
            .declared
            .is_none_or(|d| door_tail_token(return_type_desc(d)) == token);
        if agrees {
            token
        } else {
            0
        }
    }

    /// Does an `invokeWithArguments` plan do work after its dispatch: a
    /// declared primitive to box, the inner return cast, or the inner
    /// unboxing ([`inner_return_unbox_may_apply`])? Its doors arm their tail
    /// only when it does not.
    pub(crate) fn with_arguments_works_after(&self) -> bool {
        self.box_target.is_some()
            || return_cast_may_apply(self.kind, self.leaf, self.declared, None)
            || inner_return_unbox_may_apply(self.kind, self.leaf, self.declared, self.marks)
    }
}

/// [`DoorReturn`]'s steps on `result`, in the order every door took them: the
/// reference cast's refusal, the reference -> primitive unboxing, then (a
/// constructor's new object aside) the widening / narrowing and the box.
pub(crate) fn door_return_apply(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    plan: &DoorReturn<'_>,
) -> MethodCallResult {
    // A handle DECLARED `void` over a leaf that returns a value
    // (`dropReturn(target)`, `target.asType((..)void)`) discards it: `asType`'s
    // return rule, so `(Object) dropReturn(twice).invoke(3)` is `null` on
    // HotSpot 25.0.3, where the door boxed the leaf's `6`
    // (`L4W59CombinatorCallSiteTypes` row `dropReturn`; interpreter round i2
    // wave 59, lane L4, `CRATONVM_MH_VOID_DECLARED_DISCARDS`). The rest of the
    // plan then runs from the declared type.
    let discards = plan.declared.is_some_and(|d| d.ends_with(")V"))
        && !plan.leaf.ends_with(")V")
        && plan.kind != MH_KIND_CONSTRUCTOR
        && kind_has_authoritative_adapted_type(plan.kind)
        && crate::nbflags().mh_void_declared_discards;
    if discards {
        let declared = plan.declared.unwrap_or(plan.leaf);
        let discarded = DoorReturn {
            leaf: declared,
            ..*plan
        };
        return door_return_apply_steps(ctx, result.map(|_| None), &discarded);
    }
    door_return_apply_steps(ctx, result, plan)
}

/// [`door_return_apply`]'s four steps.
pub(crate) fn door_return_apply_steps(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    plan: &DoorReturn<'_>,
) -> MethodCallResult {
    if let Some(refusal) = return_cast_refusal(
        ctx,
        &result,
        plan.kind,
        plan.leaf,
        plan.declared,
        plan.cast_site,
        plan.explicit_stamp,
    ) {
        return Err(refusal);
    }
    if let Some(answer) = invoke_return_unbox(
        ctx,
        &result,
        plan.marks,
        plan.kind,
        plan.leaf,
        plan.declared,
        plan.unbox_target,
    ) {
        return answer;
    }
    // A constructor's handle already answers the new object; boxing it
    // against its `V` leaf would answer `null`.
    if plan.kind == MH_KIND_CONSTRUCTOR {
        return result;
    }
    box_return_against_target(ctx, result, plan.leaf, plan.box_target)
}

pub(crate) fn return_type_desc(desc: &str) -> &str {
    match desc_close_paren(desc) {
        Some(pos) => &desc[pos + 1..],
        None => "",
    }
}

/// Byte index of the `)` that CLOSES a method descriptor's parameter list,
/// found by walking the parameter tokens.
///
/// Not `find(')')` / `rfind(')')`: JVMS 4.2.2 bars only `. ; [ /` from a class
/// name, so `(LA)B;)LC)D;` is legal — one parameter of class `A)B`, returning
/// class `C)D` — and the first `)` cuts the parameter list short while the
/// last one cuts the return type. `cratonvm_jit_api::descriptor_param_list_end`
/// is the same walk; this crate does not depend on `jit-api`. `None` for a
/// descriptor whose parameter list never closes (malformed).
pub(crate) fn desc_close_paren(desc: &str) -> Option<usize> {
    let bytes = desc.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'('));
    while i < bytes.len() && bytes[i] != b')' {
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
    }
    (bytes.get(i) == Some(&b')')).then_some(i)
}

/// Auto-box a primitive return value if the method descriptor returns a primitive.
/// This is needed because signature-polymorphic invoke() returns Object, but the
/// underlying method may return a primitive (int, long, etc.).
pub(crate) fn auto_box_return(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    desc: &str,
) -> MethodCallResult {
    let ret_desc = return_type_desc(desc);
    match ret_desc {
        // CANONICAL — this is the signature-polymorphic `invoke` /
        // `invokeWithArguments` return path. Measured
        // `mh.invokeWithArgsInt` / `Char` / `Long` = true and
        // `mh.invokeAsObject` = true; `mhnc.asTypeFloat` = false and
        // `mhoob.asTypeInt1000` = false are the two arms
        // `box_value_canonical` delegates back to `box_value` itself.
        "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C" => match result {
            Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
            Ok(Some(val)) => Ok(Some(box_value_canonical(ctx, val, ret_desc))),
            other => other,
        },
        "V" => match result {
            // void → null for Object return, but preserve thrown exceptions.
            // Previously this arm unconditionally returned Ok(Some(Object(None))),
            // silently swallowing any Err(ExceptionThrown) from dispatch — the
            // root cause of KC26 / many Quarkus / Spring Boot rc=0 silent exits
            // when an inner native invocation raised through MethodHandle.invokeExact.
            Ok(_) => Ok(Some(Value::Object(None))),
            err => err,
        },
        // Reference / array return ('L…;' or '[…'): an Object-typed polymorphic
        // invoke MUST push a value (null at worst) — `Ok(None)` pushes NOTHING, so
        // the caller's `areturn`/consumer underflows the operand stack. This
        // happens when an adapter chain (guardWithTest / asSpreader / asType) has
        // an Object effective return type but the dispatched leaf method is `void`
        // (mh_dispatch yields `Ok(None)`). Groovy's IndyInterface is the canonical
        // case: every call site returns Object, but the resolved method (e.g.
        // `addRepositories(Closure)`) is void — `fromCache`'s
        // `cachedMethodHandle.invokeExact(args)` then underflowed at its `areturn`.
        // HotSpot's asType(...→Object) bakes a void→null filter into the adapter;
        // our asType is a passthrough, so normalize here. Thrown exceptions and a
        // present value pass through unchanged.
        _ if matches!(ret_desc.as_bytes().first(), Some(b'L') | Some(b'[')) => match result {
            Ok(None) => Ok(Some(Value::Object(None))),
            other => other,
        },
        _ => result, // void handled above; any other shape unchanged
    }
}

/// Step 1 of `i44-L4-proposal-invoke-a-jdk-delegating-handle-through-its-target`
/// (interpreter round i1 wave 45, lane L4): whether the invoke doors and
/// [`mh_dispatch_body`] dispatch a real JDK `DelegatingMethodHandle` through
/// its target. The `true` is its own last commit, so one revert restores the
/// old answer (the doors read this model's slots out of such a handle's
/// bounds and answer `null` or garbage).
pub(crate) const DELEGATING_HANDLES_DISPATCH_THROUGH_TARGET: bool = true;

/// The handle a door dispatches for `mh`: `mh` itself, or, for a real JDK
/// `java.lang.invoke.DelegatingMethodHandle` (`MethodHandleImpl$IntrinsicMethodHandle`,
/// `$WrappedMember`, `$CountingWrapper`), the handle it forwards to.
///
/// Such a handle has none of this model's slots (`object_num_fields <=
/// MH_BOUND`, the width test every reader of the slots makes), so the doors
/// used to read `MH_KIND` / `MH_DESC` out of its bounds. JDK 25 builds one in
/// `MethodHandleImpl.makeIntrinsic` (`makeArrayElementAccessor`,
/// `MethodHandles.identity`), `makeWrappedMember` and `bindCaller`. Each is
/// behaviourally its target (`DelegatingMethodHandle.getTarget()`; the
/// reinvoker `LambdaForm` calls it with the same arguments), and all four
/// permitted subclasses of the sealed class keep it in a `private final
/// MethodHandle target` field, which is read directly: no Java runs, so
/// nothing the caller holds can move. Taken only when the delegating handle's
/// `type` is its target's (then an `invokeExact` check against either agrees).
///
/// `MethodHandleImpl$AsVarargsCollector` is left alone: its `asType` COLLECTS
/// trailing arguments at another arity, which the target does not (the JDK
/// body of `asVarargsCollector` is pinned to this model's `MH_VARARGS`
/// marking). Cost on the hot path: the width compare; the rest runs only for
/// a handle that is not this model's.
pub(crate) fn mh_door_handle(ctx: &mut dyn NativeContext, mh: ObjectRef) -> ObjectRef {
    if ctx.object_num_fields(mh) > MH_BOUND || !DELEGATING_HANDLES_DISPATCH_THROUGH_TARGET {
        return mh;
    }
    mh_jdk_delegation_target(ctx, mh).unwrap_or(mh)
}

/// The `InternalError` [`mh_dispatch`] raises for a handle this model cannot
/// run (`CRATONVM_MH_FOREIGN_HANDLE_RAISES`, default on; `0` restores the
/// silent zero value). Names the handle's class, so the failing call is
/// attributable. `CRATONVM_DBG_MH_DISPATCH=1` also prints it.
#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_handle_refusal(ctx: &mut dyn NativeContext, mh: ObjectRef) -> MethodCallFailed {
    let class = ctx
        .class_name_of_id(ctx.class_id_of_object(mh))
        .unwrap_or_else(|| "?".to_string())
        .class_text(&*ctx);
    let message = format!(
        "CratonVM cannot invoke a {class}: it is not one of this VM's method handles \
         and runs a LambdaForm this VM does not execute"
    );
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_FOREIGN] {message}");
    }
    RuntimeError::InternalError { message }.into()
}

/// Run a real JDK `MethodHandle` this model has no slots for (a
/// `DirectMethodHandle`, a `BoundMethodHandle` species, ...) the way
/// `invokeBasic` runs it on HotSpot: the static method its `LambdaForm`
/// linked (`form.vmentry`, a `MemberName` of kind `REF_invokeStatic`, in a
/// pregenerated `*$Holder` or a spun `LambdaForm$MH` class), with the handle
/// itself first and then the arguments (`linkToStatic`'s layout, through
/// `native_method_handle_link_to`).
///
/// `None` -- the caller raises as before -- unless `mh` is a
/// `java.lang.invoke.MethodHandle` whose form has a static `vmentry` and the
/// arguments are as many as the handle's parameters. Interpreter round i2
/// wave 53, lane L4 (step 2 of
/// `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`), behind
/// `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY`.
///
/// Since wave 54 the arguments are converted to the handle's own parameter
/// types first, as `asType(callSiteType)` converts them on HotSpot
/// ([`adapt_leaf_args`]: a box unboxed and widened at a primitive parameter, a
/// raw primitive widened, or boxed at a reference one, a reference cast),
/// with `site` -- the call site a door armed ([`take_entry_call_site`]) -- for
/// the static types; wave 53 raised for any argument not already of the
/// parameter's variant. A site of another arity is `asType`'s
/// `WrongMethodTypeException`. Arguments already of the handle's types at the
/// handle's own site (`invokeExact`, `invokeBasic`) take no conversion.
#[cold]
#[inline(never)]
pub(crate) fn mh_run_foreign_vmentry(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
    site: Option<&str>,
) -> Option<MethodCallResult> {
    let handle_class = ctx.class_id_by_name("java/lang/invoke/MethodHandle")?;
    if !ctx.is_subclass(ctx.class_id_of_object(mh), handle_class) {
        return None;
    }
    let entry = mh_foreign_vmentry(ctx, mh)?;
    let desc = mh_declared_descriptor(ctx, mh)?;
    let (params, _ret) = split_descriptor_params(&desc)?;
    // Wave 55 (`CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK`): a real
    // `AsVarargsCollector` at a site of another type is the JDK's own
    // `asType(siteType)` (`asTypeUncached` collects), then that handle.
    if let Some(s) = site.filter(|s| *s != desc) {
        if crate::nbflags().mh_foreign_varargs_run_jdk
            && split_descriptor_params(s).is_some_and(|(p, _)| p.len() == extra_args.len())
            && ctx
                .class_name_arc_of_id(ctx.class_id_of_object(mh))
                .as_deref()
                == Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
        {
            return mh_foreign_varargs_collect(ctx, mh, extra_args, s, &desc);
        }
    }
    if let Some(site_params) = site.and_then(split_descriptor_params).map(|(p, _)| p) {
        // A real `AsVarargsCollector` COLLECTS at another arity (its `asType`),
        // which this does not model: it keeps the refusal of its caller.
        if site_params.len() != params.len()
            && site_params.len() == extra_args.len()
            && ctx
                .class_name_arc_of_id(ctx.class_id_of_object(mh))
                .as_deref()
                != Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
        {
            let shown_declared = method_type_display_in(&*ctx, &desc)?;
            let shown_site = method_type_display_in(&*ctx, site?)?;
            return Some(Err(mh_door_wmte(
                ctx,
                MhWmteRaise::AsType,
                &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
            )));
        }
    }
    if params.len() != extra_args.len() {
        return None;
    }
    let typed = params
        .iter()
        .zip(extra_args)
        .all(|(p, v)| match p.as_bytes().first() {
            Some(b'Z' | b'B' | b'C' | b'S' | b'I') => matches!(v, Value::Int(_)),
            Some(b'J') => matches!(v, Value::Long(_)),
            Some(b'F') => matches!(v, Value::Float(_)),
            Some(b'D') => matches!(v, Value::Double(_)),
            Some(b'L' | b'[') => matches!(v, Value::Object(_)),
            _ => false,
        });
    let converts = !typed || site.is_some_and(|s| s != desc);
    // An `AsVarargsCollector`'s trailing argument may be one to COLLECT, which
    // a conversion to the array parameter would refuse: wave 53's rule.
    if !typed
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(mh))
            .as_deref()
            == Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
    {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        let class = ctx
            .class_name_of_id(ctx.class_id_of_object(mh))
            .unwrap_or_default();
        let from = if converts {
            site.unwrap_or("its arguments' own types")
        } else {
            ""
        };
        let converting = if converts {
            format!(" (converting from {from})")
        } else {
            String::new()
        };
        eprintln!("[MH_FOREIGN] running {class}{desc} through its form's vmentry{converting}");
    }
    let (mh, entry, converted) = if converts {
        // A box the conversion mints is an allocation: the handle (and its
        // entry) are re-read after it; `adapt_leaf_args` re-reads the
        // arguments itself.
        let site_params = leaf_site_params(site, 0, extra_args.len());
        let pin = ctx.pin_native_root(mh);
        let converted = adapt_leaf_args(
            ctx,
            extra_args,
            &desc,
            site_params.as_deref(),
            mh_explicit_scope(),
        );
        let mh = ctx.read_native_pin(pin, mh);
        ctx.unpin_native_roots(pin);
        match converted {
            Ok(values) => (mh, mh_foreign_vmentry(ctx, mh)?, Some(values)),
            Err(refusal) => return Some(Err(refusal)),
        }
    } else {
        (mh, entry, None)
    };
    let values = converted.as_deref().unwrap_or(extra_args);
    // Wave 56 (`CRATONVM_IBG_SPIN_ON_MISS`): the JDK's VarHandle invoker over
    // one of this VM's synthetic `VarHandle`s, which its spun form cannot
    // serve (`getMethodHandle` reads the absent `vform`).
    // Wave 57 (`CRATONVM_VH_SYNTHETIC_MH_TABLE`): `getMethodHandleUncached`
    // answers a synthetic `VarHandle` at the source, whoever asks, so the
    // form runs as it is and neither screen is needed.
    let seeded: Vec<Value>;
    let (mh, entry, values) =
        if crate::nbflags().ibg_spin_on_miss && !crate::nbflags().vh_synthetic_mh_table {
            if let Some(answer) = mh_foreign_vh_invoker_over_synthetic(ctx, mh, values, &desc) {
                return Some(answer);
            }
            // ... and any other real handle that carries one (an `asType` /
            // `bindTo` of that invoker inlines its form): the synthetic
            // `VarHandle`'s `methodHandleTable` is seeded for the access modes
            // the handle's `AccessDescriptor`s name.
            match mh_foreign_seed_synthetic_vh_tables(ctx, mh, values) {
                Some((mh, again)) => {
                    seeded = again;
                    (mh, mh_foreign_vmentry(ctx, mh)?, seeded.as_slice())
                }
                None => (mh, entry, values),
            }
        } else {
            (mh, entry, values)
        };
    // Wave 55 (`CRATONVM_MH_FOREIGN_VMENTRY_FIRST`): an entry the held
    // `InvokerBytecodeGenerator.generateCustomizedCode` native fabricated
    // (`LambdaForm.MH`, a method no class declares) is no code to run: a real
    // direct handle is its `member` (`mh_real_direct_dispatch`), anything else
    // keeps its caller's refusal. Running it was a `NoSuchMethodError` naming
    // `java.lang.invoke.LambdaForm.MH(..)`.
    if crate::nbflags().mh_foreign_vmentry_first && mh_vmentry_is_fabricated(ctx, entry) {
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!(
                "[MH_FOREIGN] {desc}: its form's vmentry is fabricated; dispatching by member"
            );
        }
        return mh_real_direct_dispatch(ctx, mh, values, true);
    }
    // Wave 54 (`CRATONVM_MH_FOREIGN_DMH_BY_MEMBER`): a plain direct handle's
    // linker would only fetch its `member` and make this `linkTo*` call.
    if crate::nbflags().mh_foreign_dmh_by_member {
        if let Some((member, ensure_init)) = mh_foreign_dmh_member(ctx, mh, entry) {
            if crate::nbflags().dbg_mh_dispatch {
                let init = if ensure_init {
                    " (initializing its class first)"
                } else {
                    ""
                };
                eprintln!("[MH_FOREIGN] dispatching DirectMethodHandle{desc} by its member{init}");
            }
            let (member, values) = if ensure_init {
                // `invokeStaticInit`'s barrier, the JDK's own
                // `internalMemberNameEnsureInit` (it runs `<clinit>`): a GC
                // point, so the arguments are pinned across it.
                let (pin_base, handles) = pin_mh_args(ctx, values);
                let member = ctx.invoke(
                    "java/lang/invoke/DirectMethodHandle",
                    "internalMemberNameEnsureInit",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(mh))],
                );
                let values = read_pinned_mh_args(ctx, &handles, values);
                if pin_base != usize::MAX {
                    ctx.unpin_native_roots(pin_base);
                }
                match member {
                    Ok(Some(Value::Object(Some(m)))) => (m, values),
                    Ok(_) => return None,
                    Err(e) => return Some(Err(e)),
                }
            } else {
                (member, values.to_vec())
            };
            let mut args = values;
            args.push(Value::Object(Some(member)));
            return Some(crate::native_method_handle_link_to(ctx, &args));
        }
    }
    let mut args = Vec::with_capacity(values.len() + 2);
    args.push(Value::Object(Some(mh)));
    args.extend_from_slice(values);
    args.push(Value::Object(Some(entry)));
    Some(crate::native_method_handle_link_to(ctx, &args))
}

/// The JDK's own VarHandle invoker (`Invokers.varHandleMethodInvoker` /
/// `varHandleMethodExactInvoker`: a `BoundMethodHandle$Species_L` over a
/// `VARHANDLE_INVOKER` / `VARHANDLE_EXACT_INVOKER` form, bound to its
/// `AccessDescriptor`) called on one of this VM's synthetic `VarHandle`s,
/// which have no `vform`: the form's `checkVarHandle*Type` reaches
/// `getMethodHandleUncached`, a `NullPointerException` on `this.vform` on
/// every call. Answered as this model's invoker of the same access mode and
/// type ([`vh_invoker_model_handle`], what the registered
/// `MethodHandles.varHandleInvoker` answers), with the arguments already of
/// the handle's own types. `None` for any other handle, a real `VarHandle`
/// (its form runs), or a `null` one (the form's `NullPointerException`).
/// Interpreter round i2 wave 56, lane L4 (`CRATONVM_IBG_SPIN_ON_MISS`).
#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_vh_invoker_over_synthetic(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    values: &[Value],
    own: &str,
) -> Option<MethodCallResult> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(mh))
        .as_deref()
        != Some("java/lang/invoke/BoundMethodHandle$Species_L")
    {
        return None;
    }
    let Value::Object(Some(form)) = ctx.get_field_by_name(mh, "form") else {
        return None;
    };
    let Value::Object(Some(kind)) = ctx.get_field_by_name(form, "kind") else {
        return None;
    };
    let exact = match ctx.get_field_by_name(kind, "name") {
        Value::Object(Some(s)) => match ctx.read_string(s).as_deref() {
            Some("VARHANDLE_INVOKER") => false,
            Some("VARHANDLE_EXACT_INVOKER") => true,
            _ => return None,
        },
        _ => return None,
    };
    let Some(Value::Object(Some(vh))) = values.first().copied() else {
        return None;
    };
    if matches!(ctx.get_field_by_name(vh, "vform"), Value::Object(Some(_))) {
        return None;
    }
    let (params, ret) = split_descriptor_params(own)?;
    if params.first().map(String::as_str) != Some("Ljava/lang/invoke/VarHandle;") {
        return None;
    }
    let inner = format!("({}){ret}", params[1..].concat());
    let Value::Object(Some(ad)) = ctx.get_field_by_name(mh, "argL0") else {
        return None;
    };
    let Value::Int(ordinal) = ctx.get_field_by_name(ad, "mode") else {
        return None;
    };
    // Two Java calls and an allocation: the arguments are pinned across them.
    let (pin_base, handles) = pin_mh_args(ctx, values);
    let mode = ctx.invoke(
        "java/lang/invoke/VarHandle$AccessMode",
        "valueFromOrdinal",
        "(I)Ljava/lang/invoke/VarHandle$AccessMode;",
        &[Value::Int(ordinal)],
    );
    let invoker = match mode {
        Ok(Some(Value::Object(Some(mode)))) => {
            let method = access_mode_method_name(&access_mode_name(ctx, mode));
            if method.is_empty() {
                Ok(None)
            } else {
                vh_invoker_model_handle(ctx, &method, &inner, exact)
            }
        }
        Ok(_) => Ok(None),
        Err(e) => Err(e),
    };
    let values = read_pinned_mh_args(ctx, &handles, values);
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    match invoker {
        Ok(Some(Value::Object(Some(invoker)))) => {
            if crate::nbflags().dbg_mh_dispatch {
                let e = if exact { "exact " } else { "" };
                eprintln!("[MH_FOREIGN] the JDK's {e}VarHandle invoker{own} over a synthetic VarHandle: this model's invoker{inner}");
            }
            Some(mh_dispatch(ctx, invoker, &values))
        }
        Ok(_) => None,
        Err(e) => Some(Err(e)),
    }
}

/// A real JDK handle about to run its form's `vmentry` with one of this VM's
/// synthetic `VarHandle`s among its arguments or bound values, and a
/// `VarHandle$AccessDescriptor` beside it (an `asType` / `bindTo` of the JDK's
/// VarHandle invoker, whose edited form inlines `checkVarHandleGenericType`):
/// the form's `handle.getMethodHandle(ad.mode)` reads the `VarHandle`'s
/// `methodHandleTable` cache first and only on a miss reaches
/// `getMethodHandleUncached`, which needs the `vform` a synthetic handle does
/// not have. So the cache is seeded, for the access modes the descriptors
/// name, with what `getMethodHandleUncached` would answer: a handle of type
/// `accessModeType(mode)` with the `VarHandle` prepended that performs the
/// mode on its first argument (this model's invoker, as
/// [`mh_foreign_vh_invoker_over_synthetic`] uses). `Some` (the handle and the
/// arguments re-read: the seeding allocates and calls Java) only when an
/// entry was written. Interpreter round i2 wave 56, lane L4
/// (`CRATONVM_IBG_SPIN_ON_MISS`).
#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_seed_synthetic_vh_tables(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    values: &[Value],
) -> Option<(ObjectRef, Vec<Value>)> {
    let vh_class = ctx.class_id_by_name("java/lang/invoke/VarHandle")?;
    let ad_class = ctx.class_id_by_name("java/lang/invoke/VarHandle$AccessDescriptor")?;
    let mut vhs: Vec<ObjectRef> = Vec::new();
    let mut modes: Vec<i32> = Vec::new();
    let mut candidates: Vec<ObjectRef> = values
        .iter()
        .filter_map(|v| match v {
            Value::Object(Some(o)) => Some(*o),
            _ => None,
        })
        .collect();
    for i in 0..ctx.object_num_fields(mh) {
        if let Value::Object(Some(o)) = ctx.get_field(mh, i) {
            candidates.push(o);
        }
    }
    for o in candidates {
        if ctx.object_is_array(o) {
            continue;
        }
        let cid = ctx.class_id_of_object(o);
        if cid == ad_class {
            if let Value::Int(m) = ctx.get_field_by_name(o, "mode") {
                if !modes.contains(&m) {
                    modes.push(m);
                }
            }
        } else if (cid == vh_class || ctx.is_subclass(cid, vh_class))
            && !vhs.contains(&o)
            && !matches!(ctx.get_field_by_name(o, "vform"), Value::Object(Some(_)))
            && !vh_has_synthetic_layout(ctx, o)
        {
            vhs.push(o);
        }
    }
    if vhs.is_empty() || modes.is_empty() {
        return None;
    }
    // Every Java call and allocation below is a GC point: the handle, the
    // arguments and the `VarHandle`s are pinned for the whole seeding.
    let mh_pin = ctx.pin_native_root(mh);
    let (args_base, handles) = pin_mh_args(ctx, values);
    let vh_pins: Vec<usize> = vhs.iter().map(|vh| ctx.pin_native_root(*vh)).collect();
    let mut wrote = false;
    for (k, vh0) in vhs.iter().enumerate() {
        for &mode in &modes {
            let vh = ctx.read_native_pin(vh_pins[k], *vh0);
            wrote |= vh_seed_method_handle_table(ctx, vh, mode).is_some();
        }
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    let again = read_pinned_mh_args(ctx, &handles, values);
    ctx.unpin_native_roots(mh_pin);
    let _ = args_base;
    if !wrote {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] seeded the methodHandleTable of {} synthetic VarHandle(s) for mode(s) {modes:?}",
            vhs.len()
        );
    }
    Some((mh, again))
}

/// One entry of [`mh_foreign_seed_synthetic_vh_tables`]: `vh`'s
/// `methodHandleTable[mode]`, created (`AccessMode.COUNT` long) when absent;
/// `Some(())` when written, `None` when it already held a handle or anything
/// could not be built (the form then fails as before).
pub(crate) fn vh_seed_method_handle_table(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    mode: i32,
) -> Option<()> {
    let index = usize::try_from(mode).ok()?;
    let table = match ctx.get_field_by_name(vh, "methodHandleTable") {
        Value::Object(Some(t)) => {
            if index >= ctx.array_length(t) {
                return None;
            }
            if matches!(ctx.get_array_element(t, index), Value::Object(Some(_))) {
                return None;
            }
            None
        }
        _ => Some(()),
    };
    let pin = ctx.pin_native_root(vh);
    let answer = vh_seed_method_handle_table_pinned(ctx, pin, vh, index, table.is_some());
    ctx.unpin_native_roots(pin);
    answer
}

pub(crate) fn vh_seed_method_handle_table_pinned(
    ctx: &mut dyn NativeContext,
    pin: usize,
    vh: ObjectRef,
    index: usize,
    create: bool,
) -> Option<()> {
    let handle = vh_model_mode_handle(ctx, pin, vh, index)?;
    let handle_pin = ctx.pin_native_root(handle);
    let vh_now = ctx.read_native_pin(pin, vh);
    let table = if create {
        let count = match ctx.invoke(
            "java/lang/invoke/VarHandle$AccessMode",
            "values",
            "()[Ljava/lang/invoke/VarHandle$AccessMode;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(all)))) => ctx.array_length(all),
            _ => return None,
        };
        let component =
            crate::lang_class::reflection_component_id(ctx, "java/lang/invoke/MethodHandle");
        let table = ctx.new_ref_array(component, count);
        let vh_now = ctx.read_native_pin(pin, vh);
        ctx.set_field_by_name(vh_now, "methodHandleTable", Value::Object(Some(table)));
        table
    } else {
        match ctx.get_field_by_name(vh_now, "methodHandleTable") {
            Value::Object(Some(t)) => t,
            _ => return None,
        }
    };
    if index >= ctx.array_length(table) {
        return None;
    }
    let handle = ctx.read_native_pin(handle_pin, handle);
    ctx.set_array_element(table, index, Value::Object(Some(handle)));
    Some(())
}

/// What `VarHandle.getMethodHandleUncached(mode)` answers on HotSpot -- a
/// handle of type `accessModeType(mode)` with the `VarHandle` prepended that
/// performs the access mode on its first argument -- for one of this VM's
/// synthetic `VarHandle`s (no `vform`): this model's invoker of that mode
/// ([`vh_invoker_model_handle`]). `vh` is pinned at `pin` by the caller (the
/// two Java calls and the allocation are GC points). `None` when anything
/// cannot be built. Shared by the wave-56 table seeding and the wave-57
/// intrinsic ([`varhandle_get_method_handle_uncached`]).
pub(crate) fn vh_model_mode_handle(
    ctx: &mut dyn NativeContext,
    pin: usize,
    vh: ObjectRef,
    index: usize,
) -> Option<ObjectRef> {
    let mode = match ctx.invoke(
        "java/lang/invoke/VarHandle$AccessMode",
        "valueFromOrdinal",
        "(I)Ljava/lang/invoke/VarHandle$AccessMode;",
        &[Value::Int(index as i32)],
    ) {
        Ok(Some(Value::Object(Some(m)))) => m,
        _ => return None,
    };
    let mode_pin = ctx.pin_native_root(mode);
    let vh_now = ctx.read_native_pin(pin, vh);
    let mode_type = ctx.invoke_virtual(
        vh_now,
        "accessModeType",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(mode))],
    );
    let mode = ctx.read_native_pin(mode_pin, mode);
    let method = access_mode_method_name(&access_mode_name(ctx, mode));
    let desc = match mode_type {
        Ok(Some(Value::Object(Some(mt)))) => methodtype_to_descriptor(ctx, mt).unwrap_or_default(),
        _ => String::new(),
    };
    if method.is_empty() || desc.is_empty() {
        return None;
    }
    match vh_invoker_model_handle(ctx, &method, &desc, false) {
        Ok(Some(Value::Object(Some(h)))) => Some(h),
        _ => None,
    }
}

/// `VarHandle.getMethodHandleUncached(I)` -- a reviewed `Intrinsic`
/// registered only under `CRATONVM_VH_SYNTHETIC_MH_TABLE` (interpreter round
/// i2 wave 57, lane L4; proposal
/// `interpreter-L4-proposal-give-synthetic-varhandles-the-jdks-method-handle-table-FIXED-20261021`).
///
/// The JDK's `VarHandle.getMethodHandle(mode)` -- what
/// `Invokers.checkVarHandleGenericType` / `checkVarHandleExactType` (every
/// VarHandle invoker form, and every form that inlines one after an
/// `asType` / `bindTo`) call -- caches into `methodHandleTable` what this
/// method answers. Its body reads `vform.getMemberName(mode)`, and a
/// `VarHandle` this VM minted has no `vform`: a `NullPointerException` on
/// every miss, which wave 56 screened from the call side in
/// `mh_run_foreign_vmentry` (`mh_foreign_vh_invoker_over_synthetic`,
/// `mh_foreign_seed_synthetic_vh_tables`) for the handles that pass that
/// door. Answering here covers every consumer, whichever way it reached
/// `getMethodHandle`: for a synthetic handle, this model's invoker of the
/// mode ([`vh_model_mode_handle`]), which the JDK's own `getMethodHandle`
/// then caches; for any other receiver (a real `VarHandle`, a synthetic
/// layout), the JDK's own body (`invoke_special_bytecode_only`, which skips
/// this registration).
pub(crate) fn varhandle_get_method_handle_uncached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let (Some(Value::Object(Some(vh))), Some(Value::Int(mode))) = (args.first(), args.get(1)) {
        let (vh, mode) = (*vh, *mode);
        if let Ok(index) = usize::try_from(mode) {
            if !matches!(ctx.get_field_by_name(vh, "vform"), Value::Object(Some(_)))
                && !vh_has_synthetic_layout(ctx, vh)
            {
                let pin = ctx.pin_native_root(vh);
                let handle = vh_model_mode_handle(ctx, pin, vh, index);
                // The building is a GC point: the body below takes the
                // handle's current address.
                let vh = ctx.read_native_pin(pin, vh);
                ctx.unpin_native_roots(pin);
                if let Some(handle) = handle {
                    if crate::nbflags().dbg_mh_dispatch {
                        eprintln!(
                            "[VH_MH_TABLE] getMethodHandleUncached({mode}) of a synthetic VarHandle: this model's invoker"
                        );
                    }
                    return Ok(Some(Value::Object(Some(handle))));
                }
                return ctx.invoke_special_bytecode_only(
                    "java/lang/invoke/VarHandle",
                    "getMethodHandleUncached",
                    "(I)Ljava/lang/invoke/MethodHandle;",
                    &[Value::Object(Some(vh)), Value::Int(mode)],
                );
            }
        }
    }
    ctx.invoke_special_bytecode_only(
        "java/lang/invoke/VarHandle",
        "getMethodHandleUncached",
        "(I)Ljava/lang/invoke/MethodHandle;",
        args,
    )
}

/// A real JDK `MethodHandleImpl$AsVarargsCollector` called at `site`, a type
/// other than its own of the arguments' arity ([`mh_run_foreign_vmentry`],
/// `CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK`): what `invoke` does on HotSpot,
/// `asType(siteType)` -- the JDK's own body, whose `asTypeUncached` builds
/// (and caches) the collector, or refuses -- and then that handle, whose type
/// is the site. Interpreter round i2 wave 55, lane L4 (proposal
/// `i54-L4-proposal-every-adapter-native-hands-a-real-handle-to-the-jdk`).
/// Wave 53/54 left such a call to its caller's `InternalError`.
#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_varargs_collect(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
    site: &str,
    own: &str,
) -> Option<MethodCallResult> {
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_FOREIGN] AsVarargsCollector at {site}: asType through the JDK's own body");
    }
    let answer = mh_foreign_varargs_collect_raw(ctx, mh, extra_args, site)?;
    // A reference site (`(Object) h.invoke(..)`, `invokeWithArguments`)
    // takes the result boxed from the collector's own return, which a door
    // reading the collector as this model's slotless handle does not box.
    Some(
        if matches!(return_type_desc(site).as_bytes().first(), Some(b'L' | b'[')) {
            auto_box_return(ctx, answer, own)
        } else {
            answer
        },
    )
}

pub(crate) fn mh_foreign_varargs_collect_raw(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
    site: &str,
) -> Option<MethodCallResult> {
    // `asType` allocates (a `MethodType`, a `LambdaForm`, a spun class): the
    // arguments and the handle are pinned across it.
    let (pin_base, handles) = pin_mh_args(ctx, extra_args);
    let mh_pin = ctx.pin_native_root(mh);
    let adapted = match build_method_type_from_descriptor(ctx, site) {
        Ok(Some(mt)) => {
            let mh = ctx.read_native_pin(mh_pin, mh);
            // Wave 114: `mh_real_as_type_bytecode` reads an `asTypeCache` hit.
            mh_real_as_type_bytecode(ctx, mh, mt)
        }
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    let values = read_pinned_mh_args(ctx, &handles, extra_args);
    ctx.unpin_native_roots(mh_pin);
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    match adapted {
        // This model's collector (`asCollector` of a real handle keeps this
        // model's carrier unless `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK`): its
        // own dispatch, which the doors would have made for it.
        Ok(Some(Value::Object(Some(collector)))) if ctx.object_num_fields(collector) > MH_BOUND => {
            Some(mh_dispatch(ctx, collector, &values))
        }
        Ok(Some(Value::Object(Some(collector)))) => {
            mh_run_foreign_vmentry(ctx, collector, &values, Some(site))
        }
        Ok(_) => None,
        Err(e) => Some(Err(e)),
    }
}

/// The `member` of a real JDK `java.lang.invoke.DirectMethodHandle` (the class
/// itself, or `$Special` for the plain `invokeSpecial` linker; not
/// `$Interface`, whose linker checks its receiver, nor `$Constructor` /
/// `$Accessor`) whose form's `vmentry` is the plain `invokeStatic`,
/// `invokeStaticInit`, `invokeVirtual` or `invokeSpecial` linker
/// (`DirectMethodHandle$Holder`, or a spun form of the same kind) over a member
/// of the matching reference kind, and whether the linker's class-init barrier
/// is owed first (`invokeStaticInit`); `None` for anything else. Such a
/// linker's body is `linkTo*(args..., internalMemberName(mh))` (or
/// `internalMemberNameEnsureInit`), so the member IS the call
/// ([`mh_run_foreign_vmentry`]): an optimisation since wave 55, when the
/// linker's own `linkTo*` from bytecode reaches the native too
/// (`CRATONVM_MH_LINKTO_FROM_BYTECODE`). Reads only.
pub(crate) fn mh_foreign_dmh_member(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    entry: ObjectRef,
) -> Option<(ObjectRef, bool)> {
    const REF_INVOKE_VIRTUAL: i32 = 5;
    const REF_INVOKE_STATIC: i32 = 6;
    const REF_INVOKE_SPECIAL: i32 = 7;
    let special_class = match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(mh))
        .as_deref()
    {
        Some("java/lang/invoke/DirectMethodHandle") => false,
        Some("java/lang/invoke/DirectMethodHandle$Special") => true,
        _ => return None,
    };
    let Value::Object(Some(name)) = ctx.get_field_by_name(entry, "name") else {
        return None;
    };
    let (want, ensure_init) = match (ctx.read_string(name).as_deref(), special_class) {
        (Some("invokeStatic"), false) => (REF_INVOKE_STATIC, false),
        (Some("invokeStaticInit"), false) => (REF_INVOKE_STATIC, true),
        (Some("invokeVirtual"), false) => (REF_INVOKE_VIRTUAL, false),
        (Some("invokeSpecial"), _) => (REF_INVOKE_SPECIAL, false),
        _ => return None,
    };
    let Value::Object(Some(member)) = ctx.get_field_by_name(mh, "member") else {
        return None;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(member))
        .as_deref()
        != Some("java/lang/invoke/MemberName")
    {
        return None;
    }
    // `preparedLambdaForm` gives a statically bound `invokeVirtual` member
    // (final, private) the `invokeSpecial` linker; its member keeps its kind,
    // whose dispatch binds the same method.
    match ctx.get_field_by_name(member, "flags") {
        Value::Int(flags)
            if (flags >> 24) & 0x0F == want
                || (want == REF_INVOKE_SPECIAL && (flags >> 24) & 0x0F == REF_INVOKE_VIRTUAL) =>
        {
            Some((member, ensure_init))
        }
        _ => None,
    }
}

/// Is `entry` (a form's `vmentry`) one the held
/// `InvokerBytecodeGenerator.generate*` natives fabricated
/// (`alloc_resolved_member_name`: a static `MH` / `interpret_*` / `NFI` "of"
/// `java.lang.invoke.LambdaForm`, which declares no such method), rather than
/// a method a class really has? A real entry is in a `*$Holder` or a spun
/// `LambdaForm$MH` class. Reads only.
pub(crate) fn mh_vmentry_is_fabricated(ctx: &mut dyn NativeContext, entry: ObjectRef) -> bool {
    match ctx.get_field_by_name(entry, "clazz") {
        Value::Object(Some(mirror)) => {
            resolve_class_name_robust(ctx, mirror).as_deref() == Some("java/lang/invoke/LambdaForm")
        }
        _ => true,
    }
}

/// Does a door hand this real handle to the wave-53 `vmentry` route
/// ([`mh_run_foreign_vmentry`]) rather than to the JDK's `LambdaForm`
/// interpreter ([`mh_real_lambda_form_dispatch`])? Outside
/// `CRATONVM_PURE_BYTECODE`, with `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY` and
/// `CRATONVM_MH_FOREIGN_VMENTRY_FIRST` on, when its form's entry is real code
/// ([`mh_vmentry_is_fabricated`]). Interpreter round i2 wave 55, lane L4.
#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_vmentry_first(ctx: &mut dyn NativeContext, mh: ObjectRef) -> bool {
    if !crate::nbflags().mh_foreign_vmentry_first
        || !crate::nbflags().mh_foreign_handle_runs_vmentry
        || cratonvm_native_api::unretire::pure_bytecode()
    {
        return false;
    }
    let entry = mh_foreign_vmentry(ctx, mh);
    let first = entry.is_some_and(|entry| !mh_vmentry_is_fabricated(ctx, entry));
    if crate::nbflags().dbg_mh_dispatch {
        let class = ctx
            .class_name_of_id(ctx.class_id_of_object(mh))
            .unwrap_or_default();
        if first {
            eprintln!(
                "[MH_FOREIGN] {class} runs its form's vmentry, not the LambdaForm interpreter"
            );
        } else {
            eprintln!(
                "[MH_FOREIGN] {class}: no real form vmentry ({})",
                if entry.is_some() {
                    "fabricated"
                } else {
                    "none"
                }
            );
        }
    }
    first
}

/// A real JDK handle's `form.vmentry` when it is a `MemberName` of kind
/// `REF_invokeStatic` ([`mh_run_foreign_vmentry`]), or `None`. Reads only.
pub(crate) fn mh_foreign_vmentry(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<ObjectRef> {
    const REF_INVOKE_STATIC: i32 = 6;
    let Value::Object(Some(form)) = ctx.get_field_by_name(mh, "form") else {
        return None;
    };
    let Value::Object(Some(entry)) = ctx.get_field_by_name(form, "vmentry") else {
        return None;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(entry))
        .as_deref()
        != Some("java/lang/invoke/MemberName")
    {
        return None;
    }
    let flags = match ctx.get_field_by_name(entry, "flags") {
        Value::Int(f) => f,
        _ => return None,
    };
    ((flags >> 24) & 0x0F == REF_INVOKE_STATIC).then_some(entry)
}

/// The cold half of [`mh_door_handle`]: `Some(target)` when `mh` is a real
/// JDK `DelegatingMethodHandle` the doors may dispatch through, following
/// nested delegation (bounded).
pub(crate) fn mh_jdk_delegation_target(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<ObjectRef> {
    const AS_VARARGS_COLLECTOR: &str = "java/lang/invoke/MethodHandleImpl$AsVarargsCollector";
    let delegating = ctx.class_id_by_name("java/lang/invoke/DelegatingMethodHandle")?;
    let mut cur = mh;
    for _ in 0..8 {
        if ctx.object_num_fields(cur) > MH_BOUND {
            break;
        }
        let cid = ctx.class_id_of_object(cur);
        if cid == delegating || !ctx.is_subclass(cid, delegating) {
            break;
        }
        let class = ctx.class_name_of_id(cid).unwrap_or_default();
        if class == AS_VARARGS_COLLECTOR {
            break;
        }
        let target = match ctx.get_field_by_name(cur, "target") {
            Value::Object(Some(t)) => t,
            _ => break,
        };
        let own = mh_declared_descriptor(ctx, cur);
        let forwarded = mh_declared_descriptor(ctx, target);
        if own.is_none() || own != forwarded {
            break;
        }
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!(
                "[MH_DELEGATING] {class} {} -> target {}",
                own.as_deref().unwrap_or("?"),
                ctx.class_name_of_id(ctx.class_id_of_object(target))
                    .unwrap_or_else(|| "?".to_string()),
            );
        }
        cur = target;
    }
    (cur != mh).then_some(cur)
}

/// SBR3 wave 2 (lane mhperf): what a REAL method handle's calls need, decoded
/// once per handle instead of on every call. The doors rebuilt the same
/// descriptor `String`s, class name and member name (`link_to_method_descriptor`,
/// `resolve_class_name_robust`, `read_string`, six `get_field_by_name`) per call:
/// that was most of the 16x between `invokeExact` through a real `DirectMethodHandle`
/// and through the carrier.
///
/// Every field read is of a `final` field of the handle (`MethodHandle.type`,
/// `DirectMethodHandle.member`, `MemberName.clazz/name/type/flags`), so the record
/// is a pure function of the handle's identity.
pub(crate) struct RealHandleRecord {
    /// `type()` rendered as a descriptor: what `invokeExact` compares the call site with.
    pub(crate) declared: String,
    /// The method kinds of a `DirectMethodHandle` (`REF_invokeVirtual` .. `REF_invokeInterface`,
    /// not constructors or fields), or `None` for any other real handle.
    pub(crate) direct: Option<RealDirectTarget>,
    /// The field kinds of a `DirectMethodHandle` (`$Accessor` / `$StaticAccessor`), decoded
    /// only under `CRATONVM_MH_REAL_FIELD_RECORD` ([`mh_real_field_record_fast`]).
    pub(crate) field: Option<RealFieldTarget>,
    /// JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO`): the last
    /// linker pair [`mh_compiled_linker_pair`] answered for a compiled `invokeExact` site
    /// on this handle ([`mh_compiled_linker_pair_memo`]). Valid for the record's epoch.
    pub(crate) linker_pair: std::cell::RefCell<Option<LinkerPairMemo>>,
}

/// One [`mh_compiled_linker_pair`] answer for a handle (JIT round 21 lane reflect3): the
/// site descriptor's text, the caller class, the pair's objects (rows of the VM's rooted
/// linker table, which do not move within the epoch the record belongs to), and whether
/// the handle's class is a `BoundMethodHandle` species (the null-first-argument rule).
///
/// JIT round 21 lane reflect4 (correctness of the opt-in memo, unconditional): the site
/// text is compared by CONTENT. It was its address and length, but a compiled site's
/// descriptor lives in its `CompiledMethod`'s owned strings and is freed with the body, so
/// another site's different descriptor of the same length could reuse the address and be
/// handed this site's linker and appendix `MethodType` (whose `checkExactType` would then
/// pass for the wrong call shape).
#[derive(Clone)]
pub(crate) struct LinkerPairMemo {
    pub(crate) site: Box<str>,
    pub(crate) caller: u32,
    pub(crate) member: ObjectRef,
    pub(crate) appendix: ObjectRef,
    pub(crate) bmh_species: bool,
}

/// A real field accessor's shape (interpreter round i3 wave 73, lane L4): what
/// [`mh_real_direct_dispatch`] / [`mh_real_field_access`] re-derived on every call, all
/// of it read from `final` fields (`member`, `fieldOffset` / `staticOffset`,
/// `MemberName.flags` and `type`) or registered once per VM (the static offset's target).
pub(crate) struct RealFieldTarget {
    /// `REF_getField` .. `REF_putStatic`.
    pub(crate) ref_kind: i32,
    /// The instance slot, or the registered static offset.
    pub(crate) offset: usize,
    /// The field's descriptor (`mirror_to_descriptor` of `MemberName.type`).
    pub(crate) fdesc: String,
    /// The static field the offset was registered for; `None` for an instance field.
    pub(crate) target: Option<(ClassId, usize)>,
    /// `MemberName.flags` carries `ACC_VOLATILE` (interpreter round i3 wave 79,
    /// lane L4): the access is fenced as a volatile one ([`mh_real_field_access_resolved`]).
    pub(crate) volatile: bool,
}

pub(crate) struct RealDirectTarget {
    pub(crate) ref_kind: i32,
    pub(crate) class: String,
    /// The declaring class, resolved once (a static call then skips the by-name class lookup).
    pub(crate) owner: Option<ClassId>,
    pub(crate) name: String,
    /// The member's own `MethodType` (no receiver) as a descriptor.
    pub(crate) desc: String,
    pub(crate) params: Vec<String>,
    /// The owner an earlier static call settled (`invoke_static_settling`): once set, the
    /// call takes the native-to-Java callback memo instead of resolving by name.
    pub(crate) settled: std::cell::Cell<Option<ClassId>>,
    /// The static door's served-raise key, kept beside `settled` (interpreter round i2
    /// wave 65, lane L3; `invoke_static_settling_keyed`, the VM's to fill).
    pub(crate) door_key: cratonvm_native_api::SettledDoorKey,
    /// A `DirectMethodHandle$Interface`'s (`ref_kind` 9) `refc`, the interface every
    /// receiver must implement (`Interface.checkReceiver`); `None` for every other kind
    /// (JIT round 19 wave 1, lane mhiface; [`mh_interface_receiver_check_enabled`]).
    pub(crate) refc: Option<ClassId>,
}

pub(crate) struct RealHandleMemoLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) addr: usize,
    pub(crate) record: Option<std::rc::Rc<RealHandleRecord>>,
}

pub(crate) const REAL_HANDLE_MEMO_SLOTS: usize = 64;

thread_local! {
    /// Per-thread memo of [`RealHandleRecord`]s, indexed by the handle's address.
    ///
    /// The soundness argument is `DIRECT_LANE_MEMO`'s: nothing moves or is freed
    /// within one collection epoch, so an equal address in an equal epoch under the
    /// same VM is the same object, and the record is a function of final fields. A
    /// context that reports VM identity `0` (the mocks) is never memoised.
    pub(crate) static REAL_HANDLE_MEMO: std::cell::RefCell<Vec<RealHandleMemoLine>> =
        std::cell::RefCell::new(
            (0..REAL_HANDLE_MEMO_SLOTS)
                .map(|_| RealHandleMemoLine { vm: 0, epoch: 0, addr: 0, record: None })
                .collect(),
        );
}

pub(crate) fn real_handle_decode(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<RealHandleRecord> {
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    let declared = crate::link_to_method_descriptor(&*ctx, mt)?;
    let cid = ctx.class_id_of_object(this);
    let is_direct = ctx
        .class_name_arc_of_id(cid)
        .is_some_and(|n| n.starts_with("java/lang/invoke/DirectMethodHandle"));
    let direct = if is_direct {
        real_direct_target_decode(ctx, this)
    } else {
        None
    };
    if !is_direct && !matches!(ctx.get_field_by_name(this, "form"), Value::Object(Some(_))) {
        return None;
    }
    let field = if is_direct && direct.is_none() && mh_real_field_record_enabled() {
        real_field_target_decode(ctx, this)
    } else {
        None
    };
    Some(RealHandleRecord {
        declared,
        direct,
        field,
        linker_pair: std::cell::RefCell::new(None),
    })
}

/// `CRATONVM_MH_REAL_FIELD_RECORD` -- performance, default OFF, `truthy_word`
/// (interpreter round i3 wave 73, lane L4; proposal
/// `i72-L4-proposal-a-real-field-accessor-is-served-from-its-handle-record-20261105`):
/// see [`mh_real_field_record_fast`].
pub(crate) fn mh_real_field_record_enabled() -> bool {
    crate::nbflags().mh_real_field_record
}

/// The [`RealFieldTarget`] of a real field `DirectMethodHandle`, or `None` for
/// anything [`mh_real_field_access`] would not serve (no member, a method kind,
/// a missing or negative offset, an unregistered static offset): the ordinary
/// path then answers, its refusals included. Pure reads; no GC point.
pub(crate) fn real_field_target_decode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<RealFieldTarget> {
    let Value::Object(Some(member)) = ctx.get_field_by_name(this, "member") else {
        return None;
    };
    let Value::Int(flags) = ctx.get_field_by_name(member, "flags") else {
        return None;
    };
    let ref_kind = (flags >> 24) & 0x0F;
    if !(1..=4).contains(&ref_kind) {
        return None;
    }
    let Value::Object(Some(member_type)) = ctx.get_field_by_name(member, "type") else {
        return None;
    };
    let is_static = ref_kind == 2 || ref_kind == 4;
    let offset_field = if is_static {
        "staticOffset"
    } else {
        "fieldOffset"
    };
    let offset = match ctx.get_field_by_name(this, offset_field) {
        Value::Long(v) if v >= 0 => v as usize,
        Value::Int(v) if v >= 0 => v as usize,
        _ => return None,
    };
    let fdesc = mirror_to_descriptor(&*ctx, member_type).into_owned();
    // gci cl48: against the handle's own `staticBase` (a same-named class of
    // another loader shares the offset).
    let target = if is_static {
        Some(real_static_accessor_target(ctx, this, offset)?)
    } else {
        None
    };
    // JIT round 20 wave 8c: an instance `fieldOffset` back to its slot (a
    // byte offset under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`).
    let offset = if is_static {
        offset
    } else {
        match crate::unsafe_natives_ext::unsafe_decode_field_offset(
            ctx.unsafe_byte_field_offsets(),
            offset,
        ) {
            (slot, 0) => slot,
            _ => return None,
        }
    };
    let volatile = flags & FIELD_ACC_VOLATILE != 0;
    Some(RealFieldTarget {
        ref_kind,
        offset,
        fdesc,
        target,
        volatile,
    })
}

/// Interpreter round i3 wave 73, lane L4 (`CRATONVM_MH_REAL_FIELD_RECORD`, default
/// off): a REAL field accessor (`DirectMethodHandle$Accessor` / `$StaticAccessor`,
/// what a `Lookup` field finder answers under `--jdk-only`) called by `invokeExact`
/// at its declared type (`call_site` equal to the record's `declared`), or by
/// `invokeBasic` (`call_site` `None`, `raw_return`), is served from its memoised
/// [`RealFieldTarget`]. The ordinary path ran [`mh_real_direct_dispatch`] per call:
/// an owned class name, four by-name reads and a rendered field descriptor before
/// the access.
///
/// The same answer, in the same order: `invokeExact`'s site refusals
/// ([`mh_site_access_refusal`], then the loader half of [`mh_real_exact_refusal`],
/// [`mh_exact_loader_refusal`], all it asks of a site equal to the declared type)
/// run first, then [`mh_real_field_access_resolved`], the tail of
/// [`mh_real_field_access`] (`checkBase`'s null check and frames, the static class
/// initialization barrier, the read or write). `None` declines: the ordinary path
/// runs, nothing disturbed.
pub(crate) fn mh_real_field_record_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    raw_return: bool,
) -> Option<MethodCallResult> {
    if !mh_real_field_record_enabled() || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // A carrier handle (this model's slots) is none of this lane's: one width compare.
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let record = real_handle_record(ctx, this)?;
    let field = record.field.as_ref()?;
    match call_site {
        Some(site) => {
            if site != record.declared {
                return None;
            }
            if let Some(refusal) = mh_site_access_refusal(&*ctx, Some(site)) {
                return Some(Err(refusal));
            }
            if let Some(refusal) = mh_exact_loader_refusal(ctx, this, site) {
                return Some(Err(refusal));
            }
        }
        None if !raw_return => return None,
        None => {}
    }
    Some(mh_real_field_access_resolved(
        ctx,
        field.ref_kind,
        field.offset,
        &field.fdesc,
        field.target,
        field.volatile,
        extra,
        raw_return,
    ))
}

/// `CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE` -- performance, default OFF,
/// `truthy_word` (interpreter round i3 wave 80, lane L4; proposal
/// `i73-L4-proposal-converting-invoke-of-a-real-field-accessor-is-served-from-its-record`):
/// see [`mh_real_field_converting_serve`].
pub(crate) fn mh_real_field_converting_serve_enabled() -> bool {
    crate::nbflags().mh_real_field_converting_serve
}

/// How a getter's value reaches a converting `invoke` site
/// ([`mh_field_converting_plan`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldConvertingReturn {
    /// The site's return is the field's own type (only the receiver differs),
    /// or a reference field read at an `Object` site: the value as it is.
    Same,
    /// A primitive field at a reference site that its wrapper is assignable
    /// to: `asType`'s boxing, canonical as `Integer.valueOf` is.
    Box,
    /// A primitive field at a wider primitive site (JLS 5.1.2): the site's
    /// primitive descriptor character.
    Widen(u8),
}

/// What [`mh_real_field_converting_serve`] does for one (declared, site) pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FieldConvertingPlan {
    /// The instance getter's declared receiver descriptor, when the site's
    /// receiver parameter is another reference type: `asType` casts the
    /// receiver to it. `None` for a static getter or an equal receiver type.
    pub(crate) receiver_cast: Option<String>,
    pub(crate) ret: FieldConvertingReturn,
}

/// The primitive descriptor characters `from` widens to (JLS 5.1.2).
pub(crate) fn field_converting_widens(from: u8, to: u8) -> bool {
    match from {
        b'B' => matches!(to, b'S' | b'I' | b'J' | b'F' | b'D'),
        b'S' | b'C' => matches!(to, b'I' | b'J' | b'F' | b'D'),
        b'I' => matches!(to, b'J' | b'F' | b'D'),
        b'J' => matches!(to, b'F' | b'D'),
        b'F' => to == b'D',
        _ => false,
    }
}

/// The reference types a primitive `prim`'s wrapper is assignable to that a
/// served site may name: `Object`, the wrapper itself, `Number` for the six
/// numeric wrappers, and `Serializable` / `Comparable`, which every wrapper
/// implements.
pub(crate) fn field_converting_boxes_to(prim: u8, site_ret: &str) -> bool {
    let wrapper = match prim {
        b'Z' => "Ljava/lang/Boolean;",
        b'B' => "Ljava/lang/Byte;",
        b'S' => "Ljava/lang/Short;",
        b'C' => "Ljava/lang/Character;",
        b'I' => "Ljava/lang/Integer;",
        b'J' => "Ljava/lang/Long;",
        b'F' => "Ljava/lang/Float;",
        b'D' => "Ljava/lang/Double;",
        _ => return false,
    };
    site_ret == wrapper
        || matches!(
            site_ret,
            "Ljava/lang/Object;" | "Ljava/io/Serializable;" | "Ljava/lang/Comparable;"
        )
        || (site_ret == "Ljava/lang/Number;" && !matches!(prim, b'Z' | b'C'))
}

/// The plan for serving a GETTER whose handle type is `declared` at the
/// `invoke` site `site` (`is_static`: a `StaticAccessor`), or `None` when the
/// site needs anything but the conversions `asType` makes without
/// user-visible effects: equal arity, the receiver a reference (cast to the
/// declared class), the return the field's type, a boxing to a supertype of
/// its wrapper, a primitive widening, or a reference read at `Object`. A void
/// site, an unboxing, a narrowing and an exact site (the other lanes') decline.
/// Pure.
pub(crate) fn mh_field_converting_plan(
    declared: &str,
    site: &str,
    is_static: bool,
) -> Option<FieldConvertingPlan> {
    if declared == site {
        return None;
    }
    let (dp, dr) = split_descriptor_params(declared)?;
    let (sp, sr) = split_descriptor_params(site)?;
    let arity = usize::from(!is_static);
    if dp.len() != arity || sp.len() != arity {
        return None;
    }
    let receiver_cast = match (dp.first(), sp.first()) {
        (None, None) => None,
        (Some(d), Some(s)) if d == s => None,
        (Some(d), Some(s)) if d.starts_with('L') && (s.starts_with('L') || s.starts_with('[')) => {
            Some(d.clone())
        }
        _ => return None,
    };
    let ret = if sr == dr {
        FieldConvertingReturn::Same
    } else if dr.len() == 1 {
        let prim = dr.as_bytes()[0];
        if sr.len() == 1 {
            let to = sr.as_bytes()[0];
            if !field_converting_widens(prim, to) {
                return None;
            }
            FieldConvertingReturn::Widen(to)
        } else if field_converting_boxes_to(prim, &sr) {
            FieldConvertingReturn::Box
        } else {
            return None;
        }
    } else if (dr.starts_with('L') || dr.starts_with('[')) && sr == "Ljava/lang/Object;" {
        FieldConvertingReturn::Same
    } else {
        return None;
    };
    Some(FieldConvertingPlan { receiver_cast, ret })
}

/// A raw primitive field value widened to the site's primitive `to`
/// ([`field_converting_widens`] already admitted the pair). `Value::Int`
/// carries `byte` / `short` / `char` / `int`.
pub(crate) fn field_converting_widen(v: Value, to: u8) -> Value {
    match (v, to) {
        (Value::Int(i), b'S' | b'I') => Value::Int(i),
        (Value::Int(i), b'J') => Value::Long(i64::from(i)),
        (Value::Int(i), b'F') => Value::Float(i as f32),
        (Value::Int(i), b'D') => Value::Double(f64::from(i)),
        (Value::Long(l), b'F') => Value::Float(l as f32),
        (Value::Long(l), b'D') => Value::Double(l as f64),
        (Value::Float(f), b'D') => Value::Double(f64::from(f)),
        (other, _) => other,
    }
}

/// The declared receiver class of a real instance accessor: its `type()`'s
/// first parameter. Pure reads.
pub(crate) fn mh_real_declared_receiver_class(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<ClassId> {
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    let Value::Object(Some(ptypes)) = ctx.get_field_by_name(mt, "ptypes") else {
        return None;
    };
    if ctx.array_length(ptypes) == 0 {
        return None;
    }
    let Value::Object(Some(mirror)) = ctx.get_array_element(ptypes, 0) else {
        return None;
    };
    ctx.class_id_from_mirror(mirror)
}

/// Does `this`'s `asTypeCache` hold an adapter of EXACTLY the site's
/// `MethodType` -- the row the site-type table filed for `call_site` when the
/// JDK's `asType` ran ([`mh_site_method_type`])? The JDK's `asType` would
/// answer that adapter (`asTypeCached` compares the type by identity). Pure
/// reads; `false` whenever the table has no row (the table off, a caller of a
/// loader that is not built in, or the first call).
pub(crate) fn mh_as_type_cache_holds_site(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    call_site: &str,
) -> bool {
    if !mh_site_type_table_enabled() {
        return false;
    }
    let Some(caller) = ctx.frame_class_ids_tail(1).0.first().copied() else {
        return false;
    };
    let Some(key) = mh_site_type_key(ctx.loader_id_of_class_definite(caller), call_site) else {
        return false;
    };
    let Some(site_mt) = ctx.bound_method_type_get(&key) else {
        return false;
    };
    let Value::Object(Some(cached)) = ctx.get_field_by_name(this, "asTypeCache") else {
        return false;
    };
    matches!(ctx.get_field_by_name(cached, "type"), Value::Object(Some(t)) if t == site_mt)
}

/// Interpreter round i3 wave 80, lane L4 (`CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE`,
/// default off; proposal
/// `i73-L4-proposal-converting-invoke-of-a-real-field-accessor-is-served-from-its-record`):
/// `invoke` on a REAL field GETTER (`DirectMethodHandle$Accessor` /
/// `$StaticAccessor`, `--jdk-only`) at a site that differs from its type only
/// by `asType`'s side-effect-free conversions ([`mh_field_converting_plan`]) is
/// served from the handle's [`RealFieldTarget`]: the receiver's cast, the
/// read ([`mh_real_field_access_resolved`]: `checkBase`'s NPE, the static
/// class-initialization barrier, the volatile fence), then the boxing or
/// widening. The ordinary path ran the JDK's `asType` (a cache hit) and the
/// adapted `BoundMethodHandle`'s form, whose `invokeBasic` on the accessor is
/// a door crossing.
///
/// Served only when the handle's `asTypeCache` already holds the adapter of
/// exactly this site's `MethodType` ([`mh_as_type_cache_holds_site`]), so the
/// first call per (handle, site type) keeps the JDK path and the handle's
/// cache state is the one HotSpot leaves. A receiver of another class is
/// refused with `Class.cast`'s `Cannot cast <receiver> to <declared>`; one the
/// fast class test does not admit and the casts' judge cannot settle declines
/// to the ordinary path. `None` declines with nothing done. Needs
/// `CRATONVM_MH_REAL_FIELD_RECORD` (default on), which decodes the record.
#[inline(never)]
pub(crate) fn mh_real_field_converting_serve(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) -> Option<MethodCallResult> {
    if !mh_real_field_converting_serve_enabled() || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let site = call_site?;
    // A carrier handle (this model's slots) is none of this lane's: one width compare.
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let record = real_handle_record(ctx, this)?;
    let field = record.field.as_ref()?;
    if !matches!(field.ref_kind, 1 | 2) {
        return None;
    }
    let plan = mh_field_converting_plan(&record.declared, site, field.ref_kind == 2)?;
    if !mh_as_type_cache_holds_site(ctx, this, site) {
        return None;
    }
    // JIT round 22 wave 4, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`):
    // the generic invoker this lane skips runs `checkGenericType` (the
    // `asTypeCache` adapter, whose type IS the site's: checked above) and then
    // `checkCustomized` of THAT adapter, before the adapter's receiver cast.
    // Count it as the before-linker lanes count their handle (a `ClassCastException`
    // below is counted, as on HotSpot); the due call declines, so the JDK chain
    // customizes the adapter on HotSpot's call; a later decline takes it back.
    let adapter = match ctx.get_field_by_name(this, "asTypeCache") {
        Value::Object(Some(adapter)) => Some(adapter),
        _ => None,
    };
    let counted = match adapter {
        Some(adapter) => lane_check_customized(&*ctx, adapter)?,
        None => None,
    };
    // The cast check below may resolve a class (a GC point): the take-back
    // reads the adapter through a pin.
    let adapter_pin = match (adapter, counted) {
        (Some(adapter), Some(_)) => Some((ctx.pin_native_root(adapter), adapter)),
        _ => None,
    };
    if let Some(target) = plan.receiver_cast.as_deref() {
        let declined = match extra.first() {
            // `Class.cast` passes a null; the accessor's `checkBase` raises.
            Some(Value::Object(None)) => false,
            Some(Value::Object(Some(r))) => {
                let r = *r;
                match mh_real_declared_receiver_class(ctx, this) {
                    None => true,
                    Some(decl) => {
                        let admitted = !ctx.object_is_array(r) && {
                            let rc = ctx.class_id_of_object(r);
                            rc == decl || ctx.is_subclass(rc, decl)
                        };
                        if !admitted {
                            let recv = Value::Object(Some(r));
                            match reference_arg_admitted(ctx, recv, target) {
                                Some(false) => {
                                    let failure = reference_cast_failure(ctx, recv, target);
                                    note_reference_cast_chain(ctx);
                                    if let Some((pin, _)) = adapter_pin {
                                        ctx.unpin_native_roots(pin);
                                    }
                                    return Some(Err(failure));
                                }
                                _ => true,
                            }
                        } else {
                            false
                        }
                    }
                }
            }
            _ => true,
        };
        if declined {
            if let Some((pin, adapter)) = adapter_pin {
                let adapter = ctx.read_native_pin(pin, adapter);
                lane_uncount_customization(ctx, adapter, counted);
                ctx.unpin_native_roots(pin);
            }
            return None;
        }
    }
    if let Some((pin, _)) = adapter_pin {
        ctx.unpin_native_roots(pin);
    }
    let raw = match mh_real_field_access_resolved(
        ctx,
        field.ref_kind,
        field.offset,
        &field.fdesc,
        field.target,
        field.volatile,
        extra,
        true,
    ) {
        Ok(Some(v)) => v,
        other => return Some(other),
    };
    let value = match plan.ret {
        FieldConvertingReturn::Same => raw,
        FieldConvertingReturn::Box => {
            crate::lang_class::box_value_canonical(ctx, raw, &field.fdesc)
        }
        FieldConvertingReturn::Widen(to) => field_converting_widen(raw, to),
    };
    Some(Ok(Some(value)))
}

pub(crate) fn real_direct_target_decode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<RealDirectTarget> {
    let Value::Object(Some(member)) = ctx.get_field_by_name(this, "member") else {
        return None;
    };
    let Value::Int(flags) = ctx.get_field_by_name(member, "flags") else {
        return None;
    };
    let ref_kind = (flags >> 24) & 0x0F;
    if !matches!(ref_kind, 5 | 6 | 7 | 9) {
        return None;
    }
    // Interpreter round i5 wave 108, lane L3: a statically bound virtual or
    // interface member (`native_mhn_resolve` / `native_mhn_init` settle it as
    // `REF_invokeSpecial`, as HotSpot) on a handle that is not a
    // `$Special` is served as its request was before: by the plain handle's
    // virtual dispatch (which binds the same method) and the `$Interface`
    // handle's receiver check. Only `findSpecial` / `unreflectSpecial` mint a
    // `$Special`.
    let ref_kind = if ref_kind == 7 {
        match ctx
            .class_name_arc_of_id(ctx.class_id_of_object(this))
            .as_deref()
        {
            Some("java/lang/invoke/DirectMethodHandle") => 5,
            Some("java/lang/invoke/DirectMethodHandle$Interface") => 9,
            _ => 7,
        }
    } else {
        ref_kind
    };
    let Value::Object(Some(member_type)) = ctx.get_field_by_name(member, "type") else {
        return None;
    };
    let desc = crate::link_to_method_descriptor(&*ctx, member_type)?;
    let (params, _ret) = split_descriptor_params(&desc)?;
    let Value::Object(Some(clazz)) = ctx.get_field_by_name(member, "clazz") else {
        return None;
    };
    let class = resolve_class_name_robust(&*ctx, clazz)?;
    if class.starts_with('[') {
        return None;
    }
    let owner = ctx.class_id_from_mirror(clazz);
    let name = match ctx.get_field_by_name(member, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    if name.is_empty() {
        return None;
    }
    // JIT round 19 wave 1 (lane mhiface): the interface handle's receiver class.
    let refc = if ref_kind == 9 {
        match ctx.get_field_by_name(this, "refc") {
            Value::Object(Some(m)) => ctx.class_id_from_mirror(m),
            _ => None,
        }
    } else {
        None
    };
    Some(RealDirectTarget {
        ref_kind,
        class,
        owner,
        name,
        desc,
        params,
        settled: std::cell::Cell::new(None),
        door_key: cratonvm_native_api::SettledDoorKey::new(),
        refc,
    })
}

/// `CRATONVM_MH_INTERFACE_RECEIVER_CHECK` -- correctness, default ON, `=0`
/// restores the round 18 behaviour (JIT round 19 wave 1, lane mhiface;
/// `r19-mh-interface-handle-runs-a-non-implementors-same-named-method-FIXED-20261007.md`):
/// an unbound interface `findVirtual` handle called with a receiver whose class
/// does not implement the interface raises HotSpot's
/// `DirectMethodHandle.Interface.checkReceiver` error,
/// `IncompatibleClassChangeError: Receiver class C does not implement the
/// requested interface I`. The doors dispatched such a call by NAME
/// (`invoke_virtual_declared`), so a receiver with a same-named method ran it
/// and returned its answer. The verifier does not check interface
/// assignability, so such a receiver reaches `invokeExact` from bytecode.
/// Read only on the refusal path.
pub(crate) fn mh_interface_receiver_check_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INTERFACE_RECEIVER_CHECK")
}

/// HotSpot's `Interface.checkReceiver` refusal for `receiver` against the
/// interface `refc`, or `None` when it implements it (or `refc` is not an
/// interface, or the switch is off). See [`mh_interface_receiver_check_enabled`].
pub(crate) fn mh_interface_receiver_refusal(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    refc: ClassId,
) -> Option<MethodCallFailed> {
    if !ctx.is_interface_class(refc) {
        return None;
    }
    if ctx.object_is_array(receiver) {
        // JIT round 19 residual (the page's "arrays keep the doors' rules"):
        // an array implements only `Cloneable` and `Serializable`, so every
        // other interface refuses it as HotSpot does (`Receiver class [I does
        // not implement ...`). The doors answered `NoSuchMethodError: 'int
        // java.lang.Object.apply(int)'` (`--jdk-only`, the by-name call on the
        // array's `Object` class) or `ClassCastException` (`--compatible`).
        let refc_internal = ctx.class_name_of_id(refc)?;
        if matches!(
            refc_internal.as_str(),
            "java/lang/Cloneable" | "java/io/Serializable"
        ) || !mh_interface_receiver_check_enabled()
        {
            return None;
        }
        let recv_name = entry_value_type_display(&*ctx, receiver);
        let refc_name = refc_internal.class_text(&*ctx);
        return Some(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/IncompatibleClassChangeError",
            &format!(
                "Receiver class {recv_name} does not implement the requested interface {refc_name}"
            ),
        ));
    }
    let recv_class = ctx.class_id_of_object(receiver);
    if recv_class == refc || ctx.is_subclass(recv_class, refc) {
        return None;
    }
    if !mh_interface_receiver_check_enabled() {
        return None;
    }
    // The casts' own judge confirms the refusal (lambda proxies, synthetic
    // implementors, a name several loaders define: anything it admits or
    // cannot settle keeps the by-name dispatch, as before).
    let refc_internal = ctx.class_name_of_id(refc)?;
    if reference_arg_admitted(
        ctx,
        Value::Object(Some(receiver)),
        &format!("L{refc_internal};"),
    ) != Some(false)
    {
        return None;
    }
    let recv_name = ctx.class_name_of_id(recv_class)?.class_text(&*ctx);
    let refc_name = refc_internal.class_text(&*ctx);
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/IncompatibleClassChangeError",
        &format!(
            "Receiver class {recv_name} does not implement the requested interface {refc_name}"
        ),
    ))
}

/// [`mh_interface_receiver_refusal`] for a carrier handle, whose interface is
/// known by NAME (`MH_CLASS`): asked only when exactly one loaded class has
/// that name, so a loader-blind lookup cannot name another loader's interface.
pub(crate) fn mh_interface_receiver_refusal_named(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    class: &str,
) -> Option<MethodCallFailed> {
    if class.is_empty() || class.starts_with('[') || class == "java/lang/Object" {
        return None;
    }
    // The common case first: a receiver of exactly the named class (a class,
    // never an interface).
    if !ctx.object_is_array(receiver)
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(receiver))
            .as_deref()
            == Some(class)
    {
        return None;
    }
    let cratonvm_native_api::NameLookup::Unique(refc) = ctx.classify_class_name(class) else {
        return None;
    };
    mh_interface_receiver_refusal(ctx, receiver, refc)
}

/// The memoised [`RealHandleRecord`] of `this`, or `None` for a handle this model
/// does not decode (the caller takes its ordinary path). Pure reads; no GC point.
pub(crate) fn real_handle_record(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<std::rc::Rc<RealHandleRecord>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return real_handle_decode(ctx, this).map(std::rc::Rc::new);
    }
    let epoch = ctx.gc_address_epoch();
    let addr = this.as_ptr() as usize;
    let slot = (addr >> 3) & (REAL_HANDLE_MEMO_SLOTS - 1);
    let hit = REAL_HANDLE_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm && line.epoch == epoch && line.addr == addr {
            line.record.clone()
        } else {
            None
        }
    });
    if hit.is_some() {
        return hit;
    }
    let record = std::rc::Rc::new(real_handle_decode(ctx, this)?);
    REAL_HANDLE_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = RealHandleMemoLine {
            vm,
            epoch,
            addr,
            record: Some(record.clone()),
        };
    });
    Some(record)
}

/// The fast lane of the doors for a REAL `DirectMethodHandle` of a method kind:
/// the call site (`call_site`, or none for `invokeBasic`) is the handle's own
/// type, so `invoke` and `invokeExact` agree and no adaptation, refusal or
/// unboxing applies. `None` declines (the ordinary door runs, nothing disturbed).
///
/// The call is `linkToStatic` / `linkToVirtual` / `linkToSpecial` /
/// `linkToInterface` with this handle's member as appendix, which is what
/// `mh_real_direct_dispatch` ran (`native_method_handle_link_to`), minus the
/// per-call decode. A primitive result of the return's own `Value` variant is handed
/// back raw, as `mh_direct_lane_return` does: the caller unboxes against its site.
///
/// Interpreter round i4 wave 96, lane L4: `caller_site` says `call_site` is the
/// CALLER's own `invokeExact` / `invoke` site (the two natives), which HotSpot
/// links through the caller before the member runs. Such a site gets the
/// linker's answer ([`mh_linked_invoker`]): the site's access refusal
/// ([`mh_site_access_refusal`]) is raised here, and a site naming a class two
/// loaders define declines, so the ordinary path's loader refusals
/// (`mh_real_exact_refusal` / `mh_invoke_loader_cast_refusal` and the return
/// cast) answer in their order. Before, the lane asked neither and relied on
/// its callers having asked first, so a refusal depended on which lane served
/// the call. `false` for the JDK-internal callers (`invokeWithArguments`' lanes,
/// the `VarHandle` mode-handle lane, `invokeBasic`): their `call_site` is the
/// handle's own type, linked by no user class.
pub(crate) fn mh_real_direct_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    raw_return: bool,
    caller_site: bool,
) -> Option<MethodCallResult> {
    // The door's tail token, taken first so it never outlives this call: only a door that
    // armed it (`invokeExact`, at the site's own type) is in tail position; `invokeWithArguments`
    // and the other callers hand the result on with work of their own.
    let door_tail = if raw_return { 0u8 } else { take_mh_tail() };
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // Interpreter round i7 wave 144, lane L1 (`CRATONVM_MH_FORM_FRAMES_UNDER_DEBUGGER`):
    // while a JDWP session is attached the call runs the JDK's real forms
    // (`invokeExact_MT`, then `invokeBasic` into the handle's
    // `DirectMethodHandle$Holder` form), whose frames a step completes in.
    if ctx.debugger_observes_frames() {
        return None;
    }
    // A carrier handle (this model's slots) is none of this lane's: one width compare.
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let record = real_handle_record(ctx, this)?;
    let target = record.direct.as_ref()?;
    if let Some(site) = call_site {
        if site != record.declared {
            return None;
        }
    } else if !raw_return {
        return None;
    }
    let takes_receiver = target.ref_kind != 6;
    if extra.len() != target.params.len() + usize::from(takes_receiver) {
        return None;
    }
    // Wave 96: the caller's site is linked first (see the doc comment). Both
    // reads classify only names outside `java/`; a primitive or JDK-typed site
    // pays one scan of its descriptor.
    if let (true, Some(site)) = (caller_site, call_site) {
        if let Some(refusal) = mh_site_access_refusal(&*ctx, Some(site)) {
            return Some(Err(refusal));
        }
        if mh_descriptor_any_loader_name(site, |name| {
            matches!(
                ctx.classify_class_name(name),
                cratonvm_native_api::NameLookup::Ambiguous { .. }
            )
        }) {
            return None;
        }
    }
    // `invokeBasic` callers (the JDK's `NamedFunction`s) pass erased boxes; unbox
    // against the member's parameters as `mh_real_direct_dispatch` does.
    let unboxed: Vec<Value>;
    let args: &[Value] = if call_site.is_none()
        && extra.iter().any(|v| matches!(v, Value::Object(Some(_))))
        && target.params.iter().any(|p| p.len() == 1)
    {
        let lead = usize::from(takes_receiver);
        let mut out = Vec::with_capacity(extra.len());
        for (i, v) in extra.iter().enumerate() {
            let v = if i >= lead {
                match target.params.get(i - lead).map(String::as_str) {
                    Some(p) if p.len() == 1 && matches!(v, Value::Object(_)) => {
                        match mh_unbox_for_primitive(ctx, p, *v, None) {
                            Ok(u) => u,
                            Err(e) => return Some(Err(e)),
                        }
                    }
                    _ => *v,
                }
            } else {
                *v
            };
            out.push(v);
        }
        unboxed = out;
        &unboxed
    } else {
        extra
    };
    // The invoke is the lane's last act: a `invokeBasic` (raw) hands the result back as is,
    // and an exact call at the site's own type converts only what `transparent_leaf_call`
    // knows how to owe a remounted frame. Either way a virtual thread that parks in the
    // target unmounts, as it does on HotSpot (no native frame under a method handle).
    let tail_tok = door_tail;
    let result = match target.ref_kind {
        6 => {
            // Interpreter round i2 wave 62, lane L3: the static barrier the
            // ordinary door runs (`mh_static_method_initialize`, wave 59 L4),
            // so a failed `<clinit>` lists `DirectMethodHandle.ensureInitialized`'s
            // frames (`L3W58HandleClassInitFrames` `mh-findStatic`, `--jdk-only`).
            // An initialized owner pays one state read.
            let initialized = match mh_static_method_initialize_copy(
                ctx,
                target.owner,
                &target.class,
                target.settled.get().is_none(),
                &target.name,
                &target.desc,
                args,
            ) {
                Ok(copy) => copy,
                Err(e) => return Some(Err(e)),
            };
            let args = initialized.as_deref().unwrap_or(args);
            let transparent = if raw_return {
                None
            } else {
                transparent_leaf_call(&*ctx, tail_tok, &target.desc, None)
            };
            let (result, now_settled) = if raw_return {
                cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_static_settling_keyed(
                        target.owner,
                        target.settled.get(),
                        &target.door_key,
                        &target.class,
                        &target.name,
                        &target.desc,
                        args,
                    )
                })
            } else {
                ctx.invoke_static_settling_keyed(
                    target.owner,
                    target.settled.get(),
                    &target.door_key,
                    &target.class,
                    &target.name,
                    &target.desc,
                    args,
                )
            };
            transparent_leaf_call_done(ctx, transparent, &result);
            if now_settled.is_some() {
                target.settled.set(now_settled);
            }
            result
        }
        5 | 9 => {
            let Some(Value::Object(Some(receiver))) = args.first().copied() else {
                return Some(Err(
                    RuntimeError::NullPointerException { message: None }.into()
                ));
            };
            // JIT round 19 wave 1 (lane mhiface; `CRATONVM_MH_INTERFACE_RECEIVER_CHECK`):
            // `Interface.checkReceiver`, before the by-name dispatch.
            if let Some(refc) = target.refc {
                if let Some(e) = mh_interface_receiver_refusal(ctx, receiver, refc) {
                    return Some(Err(e));
                }
            }
            // Wave 118 (`CRATONVM_MH_VIRTUAL_BY_OWNER_ID`): by the member's
            // declaring class id (`MemberName.clazz`) when known.
            let transparent = if raw_return {
                None
            } else {
                transparent_leaf_call(&*ctx, tail_tok, &target.desc, Some(receiver))
            };
            let result = if raw_return {
                cratonvm_native_api::continuation_pin::tail_call(|| {
                    mh_invoke_virtual_by_owner(
                        ctx,
                        target.owner,
                        &target.class,
                        receiver,
                        &target.name,
                        &target.desc,
                        &args[1..],
                    )
                })
            } else {
                mh_invoke_virtual_by_owner(
                    ctx,
                    target.owner,
                    &target.class,
                    receiver,
                    &target.name,
                    &target.desc,
                    &args[1..],
                )
            };
            transparent_leaf_call_done(ctx, transparent, &result);
            result
        }
        // Interpreter round i2 wave 64, lane L4 (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`,
        // host round 2 of `L4W64SpecialByOwnerId`): THIS is the `--jdk-only` route of a
        // JDK `DirectMethodHandle$Special` -- the real-handle lane decodes its
        // `MemberName` and calls the member directly, never reaching `linkToSpecial`.
        // By the declaring class's id (`MemberName.clazz`'s mirror), not by its name:
        // the by-name `invoke_special` ran a third, application-loader copy (`mid-?`).
        _ => match target.owner {
            Some(owner) if mh_special_by_owner_id_enabled() => {
                note_special_route(
                    "real-handle-lane",
                    &target.class,
                    &target.name,
                    &target.desc,
                    true,
                );
                ctx.invoke_special_by_class_id(
                    owner,
                    &target.class,
                    &target.name,
                    &target.desc,
                    args,
                )
            }
            _ => {
                note_special_route(
                    "real-handle-lane",
                    &target.class,
                    &target.name,
                    &target.desc,
                    false,
                );
                ctx.invoke_special(&target.class, &target.name, &target.desc, args)
            }
        },
    };
    if raw_return {
        return Some(result);
    }
    Some(mh_direct_lane_return(ctx, result, &target.desc))
}

/// `CRATONVM_MH_REAL_CONSTRUCTOR_LANE` -- performance, default ON since its
/// triage (`=0` off; interpreter round i4 wave 88, lane L3; since JIT round 22
/// wave 3 it keeps `checkCustomized`'s count, `CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`): a REAL nullary `DirectMethodHandle$Constructor`
/// called by `invokeExact` at its declared type -- the `target` of every
/// `DirectConstructorHandleAccessor` of a no-argument constructor
/// (`MethodHandleAccessorFactory.makeConstructorHandle`'s `asType` to
/// `()Object` is a pure view, so the target stays a `DirectMethodHandle`) --
/// allocates `instanceClass` and runs `initMethod`'s nullary `<init>` with
/// `invokespecial` semantics through the native-instantiate funnel, which is
/// what the handle's `LF_NEWINVSPECIAL` form does
/// (`DirectMethodHandle.allocateInstance` + `linkToSpecial`). Declines (the
/// ordinary door runs, nothing disturbed) for any other handle, a call site of
/// another type, arguments, an abstract or interface instance class, and an
/// instance class not yet initialized (so a `<clinit>` failure keeps the
/// handle's own frames). A throwable from `<init>` propagates raw, as from the
/// handle. Bench: `L3W88ReflectCtorBench` (`newInstance-2`).
pub(crate) fn mh_real_constructor_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) -> Option<MethodCallResult> {
    if !crate::nbflags().mh_real_constructor_lane || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // gcj j1/jt (page `gci-i47w-oz47-jvmti-reflective-constructor-posts-no-vmobjectalloc`):
    // the funnel below allocates and runs `<init>` in one call and posts no
    // JVMTI `VMObjectAlloc`; with an agent listening the handle's own form runs,
    // whose `Unsafe.allocateInstance` posts it before `<init>`, as HotSpot's.
    if ctx.jvmti_vm_object_alloc_armed() {
        return None;
    }
    // A nearly exhausted native stack keeps the linker's StackOverflowError.
    if !extra.is_empty() || crate::lang_reflect::reflective_native_stack_exhausted() {
        return None;
    }
    // JIT round 22 wave 3, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`):
    // once a hidden-frame walk was possible, the handle's own chain runs, so
    // a `SHOW_HIDDEN_FRAMES` walk inside `<init>` sees the real (customized,
    // spun `LambdaForm$DMH`) frame, where the served call left only the
    // stand-in rows (`DirectMethodHandle$Holder.newInvokeSpecial`).
    if crate::nbflags().mh_lanes_keep_customization && !ctx.hidden_frames_never_observed() {
        return None;
    }
    // Exactly the accessor's site (`makeConstructorHandle`'s `()Object`): no
    // class of the caller's is named, so none of the linker's site refusals
    // can apply.
    let site = call_site?;
    if site != "()Ljava/lang/Object;" {
        return None;
    }
    // Wave 89, lane L3 (`CRATONVM_MH_REAL_CONSTRUCTOR_LANE_MEMO`, default off):
    // a handle this thread already served in this collection epoch skips the
    // decode below (its answer is a function of the handle's final fields and
    // of the instance class having been initialized, which does not revert).
    let memo_key = if crate::nbflags().mh_real_constructor_lane_memo && ctx.vm_identity() != 0 {
        Some((
            ctx.vm_identity(),
            ctx.gc_collection_count(),
            this.as_ptr() as usize,
        ))
    } else {
        None
    };
    if let Some(key) = memo_key {
        if let Some((inst, owner)) = ctor_lane_memo_get(key) {
            return ctor_lane_counted_instantiate(ctx, this, inst, owner);
        }
    }
    let cid = ctx.class_id_of_object(this);
    if ctx.class_name_arc_of_id(cid).as_deref()
        != Some("java/lang/invoke/DirectMethodHandle$Constructor")
    {
        return None;
    }
    // The handle's own type must be the site's (`invokeExact`).
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    if crate::link_to_method_descriptor(&*ctx, mt)?.as_str() != site {
        return None;
    }
    let Value::Object(Some(inst_mirror)) = ctx.get_field_by_name(this, "instanceClass") else {
        return None;
    };
    let Value::Object(Some(init)) = ctx.get_field_by_name(this, "initMethod") else {
        return None;
    };
    let Value::Object(Some(owner_mirror)) = ctx.get_field_by_name(init, "clazz") else {
        return None;
    };
    let inst = ctx.class_id_from_mirror(inst_mirror)?;
    let owner = ctx.class_id_from_mirror(owner_mirror)?;
    if !ctx.is_class_initialized(inst)
        || ctx.class_access_flags(inst)
            & (cratonvm_types::access_flags::ACC_ABSTRACT
                | cratonvm_types::access_flags::ACC_INTERFACE)
            != 0
    {
        return None;
    }
    if let Some(key) = memo_key {
        ctor_lane_memo_put(key, inst, owner);
    }
    ctor_lane_counted_instantiate(ctx, this, inst, owner)
}

/// Whether the door's `invokeExact` of `handle` at `site` reaches
/// [`mh_real_constructor_fast`] past its cheap gates: the switch, no
/// `VMObjectAlloc` listener, no hidden-frame walk seen, exactly the
/// accessor's `()Object` site, a `DirectMethodHandle$Constructor`. The
/// lane's own remaining checks (the handle's type, an initialized concrete
/// instance class, the stack) may still decline, to the ordinary door.
/// Interpreter round i7 follow-up (page i147-orch): the interpreter's
/// `invokeExact` frame push (`CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`) asks this
/// and keeps the door, whose native allocate-and-`<init>` is cheaper than the
/// pushed `LF_NEWINVSPECIAL` chain (L3W83 `ctor-noarg`: 7-8 us against
/// 16.5 us). Pure reads.
pub fn mh_real_constructor_lane_may_serve(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> bool {
    let flags = crate::nbflags();
    // Interpreter round i9 (page i147-orch): the typed one-argument
    // constructor's `asType` adapter keeps the door too, whose
    // [`mh_real_ctor_cast_adapter_fast`] serves it. (t1 lane H: restored; the
    // lang_invoke decomposition 2733023f27 had dropped the whole lane while
    // its switch stayed declared default ON.)
    let cast_adapter =
        site == CTOR_CAST_ADAPTER_SITE && flags.mh_real_ctor_cast_adapter_lane;
    if !(cast_adapter || (flags.mh_real_constructor_lane && site == "()Ljava/lang/Object;"))
        || flags.dbg_mh_dispatch
        || ctx.jvmti_vm_object_alloc_armed()
        || (flags.mh_lanes_keep_customization && !ctx.hidden_frames_never_observed())
    {
        return false;
    }
    if cast_adapter {
        // t1 wave 2, lane M: `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE_UNDER_JIT`
        // (opt-in) lifts the `--nojit`-only gate for an A/B.
        return (!ctx.vm_jit_enabled() || flags.mh_real_ctor_cast_adapter_lane_under_jit)
            && bound_species_ll(ctx, handle)
            && ctor_cast_adapter_decode(ctx, handle).is_some();
    }
    ctx.class_name_arc_of_id(ctx.class_id_of_object(handle)).as_deref()
        == Some("java/lang/invoke/DirectMethodHandle$Constructor")
}

/// The one call-site type [`mh_real_ctor_cast_adapter_fast`] serves.
const CTOR_CAST_ADAPTER_SITE: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

/// `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE` -- performance, default ON since
/// 2026-10-24 (`=0` off; WARNING: i9-default-flip; interpreter round i9, page
/// i147-orch; restored by t1 lane H on 2026-10-25 after the lang_invoke
/// decomposition dropped it): `invokeExact` at `(Object)Object` of the adapter
/// `MethodHandleAccessorFactory.makeConstructorHandle` returns for a
/// one-argument constructor whose parameter is a reference type other than
/// `Object` (every `$Proxy(InvocationHandler)` constructor).
///
/// `target.asType((Object)Object)` builds it in `makePairwiseConvertByEditor`:
/// a `BoundMethodHandle$Species_LL` whose `argL0` is the
/// `DirectMethodHandle$Constructor` and whose `argL1` is the argument filter
/// `MH_cast.bindTo(X.class)` (itself a `Species_LL` over the plain
/// `DirectMethodHandle` of `Class.cast`, `argL1` the class). Its form runs
/// `Class.cast` on the argument, then the constructor's `LF_NEWINVSPECIAL`
/// (`allocateInstance` + `linkToSpecial` of `initMethod`): five interpreted
/// `LambdaForm` frames and the cast before the `<init>`. This lane does the
/// same work: the cast as `Class.isInstance` (null passes), then the
/// allocation and `<init>` through `new_object_initialized_with_class_id`.
///
/// Declines (the ordinary door runs, nothing disturbed) for any other handle
/// shape, a site other than exactly `(Object)Object`, an argument the cast
/// rejects (the JDK chain then throws its own `ClassCastException`, whose
/// stack `AccessorUtils.isIllegalArgument` inspects), an abstract or
/// interface instance class, an instance class not yet initialized, an
/// `initMethod` of another class than `instanceClass`, and the gates
/// [`mh_real_constructor_fast`] keeps (a JVMTI `VMObjectAlloc` listener, a
/// hidden-frame walk seen, a nearly exhausted native stack), and a VM that
/// runs a JIT (`--nojit` only: no gain was measured with it). A throwable from
/// `<init>` propagates raw, as from the handle; the outer handle's
/// `checkCustomized` count is kept as the exact invoker keeps it.
pub(crate) fn mh_real_ctor_cast_adapter_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) -> Option<MethodCallResult> {
    let flags = crate::nbflags();
    if !flags.mh_real_ctor_cast_adapter_lane || flags.dbg_mh_dispatch {
        return None;
    }
    if extra.len() != 1 || call_site? != CTOR_CAST_ADAPTER_SITE || !bound_species_ll(&*ctx, this)
    {
        return None;
    }
    // `--nojit` only: with the JIT on the lane measured no gain (CtorBench,
    // L3W144 `proxy-ann`, L3W83 `ctor-arg`). t1 wave 2, lane M:
    // `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE_UNDER_JIT` (opt-in) lifts it
    // for an A/B on the current JIT.
    if (ctx.vm_jit_enabled() && !flags.mh_real_ctor_cast_adapter_lane_under_jit)
        || ctx.jvmti_vm_object_alloc_armed()
        || (flags.mh_lanes_keep_customization && !ctx.hidden_frames_never_observed())
        || crate::lang_reflect::reflective_native_stack_exhausted()
    {
        return None;
    }
    let (inst, cast, init_desc) = ctor_cast_adapter_decode(&*ctx, this)?;
    let arg = extra[0];
    match arg {
        Value::Object(None) => {}
        Value::Object(Some(obj)) => {
            // t1 lane H: an array's header holds its COMPONENT's class id, so
            // `class_id_of_object` of a `String[]` answered `String` and the
            // lane accepted it for a `CharSequence` parameter (HotSpot: the
            // cast fails, `IllegalArgumentException`). An array argument
            // declines to the JDK chain.
            if ctx.object_is_array(obj) || !ctx.is_subclass(ctx.class_id_of_object(obj), cast) {
                return None;
            }
        }
        _ => return None,
    }
    // Counted before `<init>` runs, as `invokeExact_MT` counts before its
    // `invokeBasic`; from here on the lane answers, so nothing is taken back.
    lane_check_customized(&*ctx, this)?;
    Some(ctx.new_object_initialized_with_class_id(inst, &init_desc, &[arg]))
}

/// `(instance class, cast class, <init> descriptor)` of the
/// [`mh_real_ctor_cast_adapter_fast`] shape, or `None` for any other handle.
/// Pure reads, remembered per thread by `(VM, collection epoch, handle
/// address)`, declines too: the answer is a function of the handles' final
/// fields and of the instance class having been initialized, which does not
/// revert (a decline for an instance class not initialized yet is not
/// remembered). The declines matter: every `Method.invoke` of a one-argument
/// static method asks this from the interpreter's `invokeExact` push at the
/// same `(Object)Object` site, of a `Species_LL` adapter of its own.
fn ctor_cast_adapter_decode(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ClassId, ClassId, std::sync::Arc<str>)> {
    let key = (ctx.vm_identity(), ctx.gc_collection_count(), this.as_ptr() as usize);
    let slot = (key.2 >> 3) & (CTOR_CAST_ADAPTER_MEMO_SLOTS - 1);
    if key.0 != 0 {
        let hit = CTOR_CAST_ADAPTER_MEMO.with(|m| {
            let m = m.borrow();
            let (k, inst, cast, desc) = &m[slot];
            (*k == key).then(|| {
                desc.clone()
                    .map(|d| (ClassId::new(*inst), ClassId::new(*cast), d))
            })
        });
        if let Some(hit) = hit {
            return hit;
        }
    }
    let answer = match ctor_cast_adapter_decode_uncached(ctx, this) {
        Ok(answer) => Some(answer),
        Err(CtorCastAdapterDecline::NotYet) => return None,
        Err(CtorCastAdapterDecline::Never) => None,
    };
    if key.0 != 0 {
        CTOR_CAST_ADAPTER_MEMO.with(|m| {
            m.borrow_mut()[slot] = match &answer {
                Some((inst, cast, desc)) => {
                    (key, inst.as_u32(), cast.as_u32(), Some(desc.clone()))
                }
                None => (key, 0, 0, None),
            };
        });
    }
    answer
}

/// Why [`ctor_cast_adapter_decode_uncached`] declined.
enum CtorCastAdapterDecline {
    /// Not the shape: final for the handle's life.
    Never,
    /// The instance class is not initialized yet.
    NotYet,
}

fn ctor_cast_adapter_decode_uncached(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(ClassId, ClassId, std::sync::Arc<str>), CtorCastAdapterDecline> {
    let (inst, cast, init_desc) =
        ctor_cast_adapter_shape(ctx, this).ok_or(CtorCastAdapterDecline::Never)?;
    if !ctx.is_class_initialized(inst) {
        return Err(CtorCastAdapterDecline::NotYet);
    }
    Ok((inst, cast, init_desc))
}

/// The structural part of [`ctor_cast_adapter_decode_uncached`]: every read
/// is of a final field. Cheapest test first (`argL0`'s class: a method
/// accessor's adapter declines there).
fn ctor_cast_adapter_shape(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ClassId, ClassId, std::sync::Arc<str>)> {
    let class_named = |o: ObjectRef, name: &str| {
        ctx.class_name_arc_of_id(ctx.class_id_of_object(o)).as_deref() == Some(name)
    };
    let obj_field = |o: ObjectRef, name: &str| match ctx.get_field_by_name(o, name) {
        Value::Object(Some(v)) => Some(v),
        _ => None,
    };
    // `argL0`: the constructor handle, one reference (non-array) parameter.
    let ctor = obj_field(this, "argL0")?;
    if !class_named(ctor, "java/lang/invoke/DirectMethodHandle$Constructor") {
        return None;
    }
    if crate::link_to_method_descriptor(ctx, obj_field(this, "type")?)?.as_str()
        != CTOR_CAST_ADAPTER_SITE
    {
        return None;
    }
    let ctor_desc = crate::link_to_method_descriptor(ctx, obj_field(ctor, "type")?)?;
    let param = &ctor_desc[1..ctor_desc.find(')')?];
    if !param.starts_with('L') || param.find(';')? + 1 != param.len() {
        return None;
    }
    // `argL1`: `MH_cast.bindTo(X.class)`.
    let filter = obj_field(this, "argL1")?;
    if !bound_species_ll(ctx, filter) {
        return None;
    }
    let cast_mh = obj_field(filter, "argL0")?;
    if !class_named(cast_mh, "java/lang/invoke/DirectMethodHandle") {
        return None;
    }
    let member = obj_field(cast_mh, "member")?;
    let member_class = ctx.class_id_from_mirror(obj_field(member, "clazz")?)?;
    if ctx.class_name_arc_of_id(member_class).as_deref() != Some("java/lang/Class")
        || ctx.read_string(obj_field(member, "name")?).as_deref() != Some("cast")
    {
        return None;
    }
    let cast = ctx.class_id_from_mirror(obj_field(filter, "argL1")?)?;
    // t1 wave 8, lane I147: the cast must be the one `asType` puts in front
    // of the constructor, `MH_cast.bindTo(X)` for its parameter `X`.
    // `filterReturnValue(ctor, castFilter)` builds the same fields (a
    // `Species_LL`: `argL0` the constructor, `argL1` the filter) for a
    // `C(Object)` constructor, but casts the RESULT: serving it would test
    // the argument and skip the result's `ClassCastException`.
    if !ctor_cast_names_param(ctx.class_name_arc_of_id(cast).as_deref(), param) {
        return None;
    }
    let inst = ctx.class_id_from_mirror(obj_field(ctor, "instanceClass")?)?;
    let owner = ctx.class_id_from_mirror(obj_field(obj_field(ctor, "initMethod")?, "clazz")?)?;
    if owner != inst
        || ctx.class_access_flags(inst)
            & (cratonvm_types::access_flags::ACC_ABSTRACT
                | cratonvm_types::access_flags::ACC_INTERFACE)
            != 0
    {
        return None;
    }
    let init_desc: std::sync::Arc<str> = format!("({param})V").into();
    Some((inst, cast, init_desc))
}

/// Does the internal name `cast` (`None`: unknown) name the class of the
/// constructor parameter descriptor `param` (`Lpkg/X;`)? The asType adapter's
/// filter casts to exactly that class ([`ctor_cast_adapter_shape`]). A
/// return-value filter over a `C(Object)` constructor passes only with
/// `java/lang/Object`, whose cast never fails, so serving it is the same.
/// By name: only the bootstrap loader may define `java/lang/Object`, and an
/// asType filter's class IS the parameter's.
pub(crate) fn ctor_cast_names_param(cast: Option<&str>, param: &str) -> bool {
    match (cast, param.strip_prefix('L').and_then(|p| p.strip_suffix(';'))) {
        (Some(cast), Some(name)) => cast == name,
        _ => false,
    }
}

const CTOR_CAST_ADAPTER_MEMO_SLOTS: usize = 16;

type CtorCastAdapterLine = (CtorLaneKey, u32, u32, Option<std::sync::Arc<str>>);

thread_local! {
    /// [`ctor_cast_adapter_decode`]'s answers (interpreter round i9),
    /// direct-mapped by handle address; `None` in the last place is a
    /// remembered decline.
    static CTOR_CAST_ADAPTER_MEMO: std::cell::RefCell<[CtorCastAdapterLine; CTOR_CAST_ADAPTER_MEMO_SLOTS]> =
        const { std::cell::RefCell::new([const { ((0, 0, 0), 0, 0, None) }; CTOR_CAST_ADAPTER_MEMO_SLOTS]) };
}

/// [`ctor_lane_instantiate`] after `Invokers.checkCustomized(this)`'s count
/// ([`lane_check_customized`]; JIT round 22 wave 3, lane runtime): the due
/// call declines to the linker, which customizes the handle; a decline of the
/// funnel takes the count back. The count is written before `<init>` runs, as
/// `invokeExact_MT` counts before its `invokeBasic`.
pub(crate) fn ctor_lane_counted_instantiate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    inst: ClassId,
    owner: ClassId,
) -> Option<MethodCallResult> {
    let counted = lane_check_customized(&*ctx, this)?;
    // `instantiate` may collect: the handle is pinned for the take-back.
    let pin = counted.map(|_| ctx.pin_native_root(this));
    let answer = ctor_lane_instantiate(ctx, inst, owner);
    if let Some(pin) = pin {
        let this_now = ctx.read_native_pin(pin, this);
        if answer.is_none() {
            lane_uncount_customization(ctx, this_now, counted);
        }
        ctx.unpin_native_roots(pin);
    }
    answer
}

/// The constructor lane's allocation and `<init>` (see [`mh_real_constructor_fast`]).
pub(crate) fn ctor_lane_instantiate(
    ctx: &mut dyn NativeContext,
    inst: ClassId,
    owner: ClassId,
) -> Option<MethodCallResult> {
    match ctx.instantiate(
        inst,
        cratonvm_native_api::registry::NativeConstruct::AncestorInit(owner),
    ) {
        Ok(None) => None,
        Ok(Some(obj)) => Some(Ok(Some(Value::Object(Some(obj))))),
        Err(e) => Some(Err(e)),
    }
}

/// `(VM identity, collection epoch, handle address)`: the soundness argument
/// of `REAL_HANDLE_MEMO` (nothing moves or is freed within one epoch).
pub(crate) type CtorLaneKey = (usize, u64, usize);

pub(crate) const CTOR_LANE_MEMO_SLOTS: usize = 16;

thread_local! {
    /// [`mh_real_constructor_fast`]'s decode per handle (wave 89, lane L3):
    /// `(key, instance class, <init> owner)`, direct-mapped by address.
    pub(crate) static CTOR_LANE_MEMO: std::cell::RefCell<[(CtorLaneKey, u32, u32); CTOR_LANE_MEMO_SLOTS]> =
        const { std::cell::RefCell::new([((0, 0, 0), 0, 0); CTOR_LANE_MEMO_SLOTS]) };
}

pub(crate) fn ctor_lane_memo_get(key: CtorLaneKey) -> Option<(ClassId, ClassId)> {
    let slot = (key.2 >> 3) & (CTOR_LANE_MEMO_SLOTS - 1);
    CTOR_LANE_MEMO.with(|m| {
        let (k, inst, owner) = m.borrow()[slot];
        (k == key && key.0 != 0).then(|| (ClassId::new(inst), ClassId::new(owner)))
    })
}

pub(crate) fn ctor_lane_memo_put(key: CtorLaneKey, inst: ClassId, owner: ClassId) {
    let slot = (key.2 >> 3) & (CTOR_LANE_MEMO_SLOTS - 1);
    CTOR_LANE_MEMO.with(|m| m.borrow_mut()[slot] = (key, inst.as_u32(), owner.as_u32()));
}

/// `CRATONVM_MH_REAL_CTOR_WIDENING_SERVE` -- performance, default ON since its
/// triage (`=0` off; `NativeFlags::mh_real_ctor_widening_serve`)
/// (interpreter round i4 wave 88, lane L4; page
/// `i88-L4-find-constructor-first-use-cost`): `invoke()` of a REAL nullary
/// `DirectMethodHandle$Constructor` at an `()Object` call site.
///
/// The JDK runs that call as the generic invoker's `checkGenericType` ->
/// `asType(()Object)` (a `viewAsType` copy of the same constructor handle: the
/// return widens, nothing converts) -> `invokeBasic` of the
/// `LF_NEWINVSPECIAL` form (`allocateInstance` + `linkToSpecial` of
/// `initMethod`). Measured (`L4W88FindConstructorSplitBench`, w86a
/// `--jdk-only`): ~200 us per call, a SECOND `invoke()` of one handle included,
/// against ~1 us on HotSpot. This serves the call as the form does, through
/// the native-instantiate funnel L3's [`mh_real_constructor_fast`]
/// (`CRATONVM_MH_REAL_CONSTRUCTOR_LANE`, the `invokeExact` twin at the declared
/// type) uses: allocate `instanceClass`, run `initMethod`'s nullary `<init>`
/// with `invokespecial` semantics, answer the object. A throwable from
/// `<init>` propagates raw, as from the handle (the form's frames are hidden).
///
/// Declines (the ordinary door runs, nothing disturbed) for any other
/// handle class, arguments, a site other than exactly `()Ljava/lang/Object;`,
/// an abstract / interface instance class, and an instance class not yet
/// initialized (its `<clinit>` failure keeps `allocateInstance`'s frames).
/// What it skips is not Java-visible: the handle's `asTypeCache` is not
/// written (the adapter the generic invoker would cache is never exposed)
/// and `customizationCount` does not move.
pub(crate) fn mh_real_ctor_widening_serve(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) -> Option<MethodCallResult> {
    if !crate::nbflags().mh_real_ctor_widening_serve || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // gcj j1/jt: as `mh_real_constructor_fast`, the handle's own form posts
    // the JVMTI `VMObjectAlloc` this funnel would not.
    if ctx.jvmti_vm_object_alloc_armed() {
        return None;
    }
    if !extra.is_empty() || call_site? != "()Ljava/lang/Object;" {
        return None;
    }
    // JIT round 22 wave 4, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`):
    // as the `invokeExact` constructor lane, once this VM may have seen a hidden
    // frame the JDK chain runs, so a `SHOW_HIDDEN_FRAMES` walk inside `<init>`
    // sees the generic invoker's (and, after 127 calls, the customized
    // `asTypeCache` adapter's) real frames, not the stand-in holder row.
    if crate::nbflags().mh_lanes_keep_customization && !ctx.hidden_frames_never_observed() {
        return None;
    }
    let cid = ctx.class_id_of_object(this);
    if ctx.class_name_arc_of_id(cid).as_deref()
        != Some("java/lang/invoke/DirectMethodHandle$Constructor")
    {
        return None;
    }
    // Nullary, returning the instance class (any reference return widens to
    // the site's `Object`).
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    if !crate::link_to_method_descriptor(&*ctx, mt)?.starts_with("()L") {
        return None;
    }
    let Value::Object(Some(inst_mirror)) = ctx.get_field_by_name(this, "instanceClass") else {
        return None;
    };
    let Value::Object(Some(init)) = ctx.get_field_by_name(this, "initMethod") else {
        return None;
    };
    let Value::Object(Some(owner_mirror)) = ctx.get_field_by_name(init, "clazz") else {
        return None;
    };
    let inst = ctx.class_id_from_mirror(inst_mirror)?;
    let owner = ctx.class_id_from_mirror(owner_mirror)?;
    if !ctx.is_class_initialized(inst)
        || ctx.class_access_flags(inst)
            & (cratonvm_types::access_flags::ACC_ABSTRACT
                | cratonvm_types::access_flags::ACC_INTERFACE)
            != 0
    {
        return None;
    }
    match ctx.instantiate(
        inst,
        cratonvm_native_api::registry::NativeConstruct::AncestorInit(owner),
    ) {
        Ok(None) => None,
        Ok(Some(obj)) => Some(Ok(Some(Value::Object(Some(obj))))),
        Err(e) => Some(Err(e)),
    }
}

/// `CRATONVM_MH_LF_VMENTRY` -- default ON (SBR3 wave 2, lane mhperf): a real
/// handle that is not a method-kind `DirectMethodHandle` runs its `LambdaForm`'s
/// `vmentry` (the spun class's static entry, or the JDK's interpreter entry
/// point), as HotSpot's `invokeBasic` does, instead of the door boxing every
/// argument into an `Object[]` for `LambdaForm.interpretWithArguments`. `0`/`false`/
/// `off`/`no` restores the interpreter path. Read once.
pub(crate) fn mh_lf_vmentry_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_LF_VMENTRY"))
}

/// A `LambdaForm.vmentry` `MemberName` decoded: the spun (or interpreter-entry)
/// class's static method and its basic-type descriptor.
pub(crate) struct LfEntryRecord {
    pub(crate) class_id: ClassId,
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    /// First character of every parameter token (`L`, `[` -> reference).
    pub(crate) kinds: Vec<u8>,
    /// As [`RealDirectTarget::settled`].
    pub(crate) settled: std::cell::Cell<Option<ClassId>>,
    /// As [`RealDirectTarget::door_key`].
    pub(crate) door_key: cratonvm_native_api::SettledDoorKey,
}

pub(crate) struct LfEntryMemoLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) addr: usize,
    pub(crate) record: Option<std::rc::Rc<LfEntryRecord>>,
}

thread_local! {
    /// Per-thread memo of [`LfEntryRecord`]s indexed by the `vmentry` `MemberName`'s
    /// address, valid for one collection epoch under one VM (the argument of
    /// [`REAL_HANDLE_MEMO`]). A form that is recompiled gets a NEW `MemberName`
    /// (`vmentry` is assigned a fresh one), which misses.
    pub(crate) static LF_ENTRY_MEMO: std::cell::RefCell<Vec<LfEntryMemoLine>> =
        std::cell::RefCell::new(
            (0..REAL_HANDLE_MEMO_SLOTS)
                .map(|_| LfEntryMemoLine { vm: 0, epoch: 0, addr: 0, record: None })
                .collect(),
        );
}

pub(crate) fn lf_entry_decode(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<LfEntryRecord> {
    let Value::Object(Some(clazz)) = ctx.get_field_by_name(mn, "clazz") else {
        return None;
    };
    let class_id = ctx.class_id_from_mirror(clazz)?;
    let class = ctx.class_name_of_id(class_id)?;
    let name = match ctx.get_field_by_name(mn, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mn, "type") else {
        return None;
    };
    let desc = crate::link_to_method_descriptor(&*ctx, mt)?;
    let (params, _) = split_descriptor_params(&desc)?;
    let kinds = params.iter().map(|p| p.as_bytes()[0]).collect();
    Some(LfEntryRecord {
        class_id,
        class,
        name,
        desc,
        kinds,
        settled: std::cell::Cell::new(None),
        door_key: cratonvm_native_api::SettledDoorKey::new(),
    })
}

pub(crate) fn lf_entry_record(
    ctx: &mut dyn NativeContext,
    mn: ObjectRef,
) -> Option<std::rc::Rc<LfEntryRecord>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return lf_entry_decode(ctx, mn).map(std::rc::Rc::new);
    }
    let epoch = ctx.gc_address_epoch();
    let addr = mn.as_ptr() as usize;
    let slot = (addr >> 3) & (REAL_HANDLE_MEMO_SLOTS - 1);
    let hit = LF_ENTRY_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm && line.epoch == epoch && line.addr == addr {
            line.record.clone()
        } else {
            None
        }
    });
    if hit.is_some() {
        return hit;
    }
    let record = std::rc::Rc::new(lf_entry_decode(ctx, mn)?);
    LF_ENTRY_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = LfEntryMemoLine {
            vm,
            epoch,
            addr,
            record: Some(record.clone()),
        };
    });
    Some(record)
}

/// Is `obj` a `java.lang.Boolean` box? (See the `I` arm of
/// [`mh_real_lf_entry_fast`].)
pub(crate) fn lf_entry_is_boolean_box(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .is_some_and(|name| &*name == "java/lang/Boolean")
}

/// Run `this.form.vmentry(this, extra...)`: what `invokeBasic` is in HotSpot.
/// `None` declines (no entry yet, an arity this model does not line up): the
/// interpreter-based door runs instead, nothing disturbed. Arguments the JDK's
/// `NamedFunction`s pass boxed are unboxed against the entry's basic parameter
/// types; the result is the entry's own (raw for a primitive).
pub(crate) fn mh_real_lf_entry_fast(
    call_site_known: bool,
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    raw_return: bool,
) -> Option<MethodCallResult> {
    if !mh_lf_vmentry_enabled() || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let Value::Object(Some(form)) = ctx.get_field_by_name(this, "form") else {
        return None;
    };
    let Value::Object(Some(mn)) = ctx.get_field_by_name(form, "vmentry") else {
        return None;
    };
    let entry = lf_entry_record(ctx, mn)?;
    if entry.kinds.len() != extra.len() + 1 || entry.kinds.first() != Some(&b'L') {
        return None;
    }
    let mut args: Vec<Value> = Vec::with_capacity(extra.len() + 1);
    args.push(Value::Object(Some(this)));
    for (v, k) in extra.iter().zip(entry.kinds[1..].iter()) {
        let v = match (*k, v) {
            // A `LambdaForm` types a `boolean` parameter `I` (its basic type for
            // `boolean`, `byte`, `short`, `char` and `int` alike), so a boxed
            // `Boolean` reaches an `I` slot legitimately; the widening unbox
            // below refuses it (`Cannot cast Boolean to Integer`), which broke
            // every Groovy indy call site that passes a `boolean`
            // (RestAssured's `RequestSpecificationImpl.<init>`).
            (b'I', Value::Object(Some(obj))) if lf_entry_is_boolean_box(&*ctx, *obj) => {
                Value::Int(match crate::lang_class::unbox_value(&*ctx, *obj) {
                    Value::Int(i) => (i != 0) as i32,
                    Value::Long(l) => (l != 0) as i32,
                    _ => 0,
                })
            }
            (b'I', Value::Object(_)) => match mh_unbox_for_primitive(ctx, "I", *v, None) {
                Ok(u) => u,
                Err(e) => return Some(Err(e)),
            },
            (b'J', Value::Object(_)) => match mh_unbox_for_primitive(ctx, "J", *v, None) {
                Ok(u) => u,
                Err(e) => return Some(Err(e)),
            },
            (b'F', Value::Object(_)) => match mh_unbox_for_primitive(ctx, "F", *v, None) {
                Ok(u) => u,
                Err(e) => return Some(Err(e)),
            },
            (b'D', Value::Object(_)) => match mh_unbox_for_primitive(ctx, "D", *v, None) {
                Ok(u) => u,
                Err(e) => return Some(Err(e)),
            },
            _ => *v,
        };
        args.push(v);
    }
    // A raw return (`invokeBasic` from a `LambdaForm`) is a tail call on HotSpot: the
    // result is handed back unchanged, so a virtual thread that parks in the entry
    // unmounts (`continuation_pin::tail_call`). The settled-method memo is a cache.
    let call = |ctx: &mut dyn NativeContext| {
        ctx.invoke_static_settling_keyed(
            Some(entry.class_id),
            entry.settled.get(),
            &entry.door_key,
            &entry.class,
            &entry.name,
            &entry.desc,
            &args,
        )
    };
    let (result, now_settled) = if raw_return {
        cratonvm_native_api::continuation_pin::tail_call(|| call(&mut *ctx))
    } else {
        call(&mut *ctx)
    };
    if now_settled.is_some() {
        entry.settled.set(now_settled);
    }
    if raw_return {
        return Some(result);
    }
    // Without the call site's descriptor the caller may expect the boxed answer
    // (`invoke` at `(..)Object` over an `(..)int` handle): box, as the interpreter
    // path always did. With it, `this`'s own type IS the site's (an `invoke` was
    // adapted before it got here), so a raw primitive is what the site reads.
    if !call_site_known {
        return Some(auto_box_return(ctx, result, &entry.desc));
    }
    Some(mh_direct_lane_return(ctx, result, &entry.desc))
}

/// SBR2 wave 10 (PUREVH), opt-in `CRATONVM_PURE_BYTECODE=1`: a REAL
/// `DirectMethodHandle` -- what `Lookup.findStatic` / `findVirtual` /
/// `findConstructor` / `findGetter` ... return when their own bytecode runs --
/// carries none of this file's synthetic slots, so every door answered `null`
/// for it (`mh 0`). Its `member` (`MemberName`) says everything a direct handle
/// is: method kinds go through the `linkTo*` native with the member as
/// appendix, a constructor allocates first, and a field kind reads or writes
/// the slot the `Accessor`'s `fieldOffset` names (an instance slot index; a
/// static is the registered static offset). `None` when the receiver is not
/// such a handle or the lever is off, so every other receiver keeps its path.
///
/// Boxed arguments are unboxed against the member's own parameter types (the
/// JDK's `NamedFunction` calls handles with erased `Object`s), and the result
/// is boxed as the doors box theirs.
pub(crate) fn mh_real_direct_dispatch(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    raw_return: bool,
) -> Option<MethodCallResult> {
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let cid = ctx.class_id_of_object(this);
    let cname = ctx.class_name_of_id(cid)?;
    if !cname.starts_with("java/lang/invoke/DirectMethodHandle") {
        return None;
    }
    // Interpreter round i7 wave 144, lane L1 (`CRATONVM_MH_FORM_FRAMES_UNDER_DEBUGGER`):
    // no direct `linkTo*` serve while a JDWP session is attached; the
    // handle's form runs (`mh_invoke_basic`'s `vmentry` run).
    if ctx.debugger_observes_frames() {
        return None;
    }
    // Wave 55 (`CRATONVM_MH_FOREIGN_VMENTRY_FIRST`): the two kinds whose linker
    // checks the receiver before its `linkTo*` (`$Interface`, and `$Special`
    // over an interface caller: `checkReceiver`'s `NullPointerException` /
    // `IncompatibleClassChangeError` texts) run that linker when it is real
    // code (`mh_run_foreign_vmentry`); the member alone skips the check.
    if matches!(
        cname.as_str(),
        "java/lang/invoke/DirectMethodHandle$Interface"
            | "java/lang/invoke/DirectMethodHandle$Special"
    ) && mh_foreign_vmentry_first(ctx, this)
    {
        return None;
    }
    // ... and when the linker is not real code (its form is not pregenerated,
    // and the held generator fabricated its entry), the JDK's own
    // `checkReceiver` runs here, before the member, as the linker would run it.
    let mut this = this;
    let mut extra_checked: Option<Vec<Value>> = None;
    if crate::nbflags().mh_foreign_vmentry_first
        && !extra.is_empty()
        && mh_linker_checks_receiver(ctx, this, &cname)
    {
        // A Java call (a GC point): the handle and the arguments are pinned.
        let (pin_base, handles) = pin_mh_args(ctx, extra);
        let this_pin = ctx.pin_native_root(this);
        let checked = ctx.invoke_virtual_bytecode_only(
            this,
            "checkReceiver",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[extra[0]],
        );
        this = ctx.read_native_pin(this_pin, this);
        extra_checked = Some(read_pinned_mh_args(ctx, &handles, extra));
        ctx.unpin_native_roots(this_pin);
        if pin_base != usize::MAX {
            ctx.unpin_native_roots(pin_base);
        }
        if let Err(e) = checked {
            return Some(Err(e));
        }
    }
    let extra: &[Value] = extra_checked.as_deref().unwrap_or(extra);
    let member = match ctx.get_field_by_name(this, "member") {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let flags = match ctx.get_field_by_name(member, "flags") {
        Value::Int(f) => f,
        _ => return None,
    };
    let ref_kind = (flags >> 24) & 0x0F;
    // Interpreter round i2 wave 62, lane L3: an allocator handle's instance
    // class is initialized first, failing as `DirectMethodHandle.allocateInstance`'s
    // `Unsafe.allocateInstance` fails, with its frames
    // (`DmhInitBarrier::Constructor`). The `instantiate(..).ok()` below
    // swallowed the `ExceptionInInitializerError`, and the by-name fallback
    // then declined on the class's error state, so the caller saw only the
    // next door's frameless `NoClassDefFoundError`
    // (`L4W59HandleClassInitFrames` `mh-constructor`, `--jdk-only`). The
    // handle, its member and the arguments are pinned across the `<clinit>`.
    let mut member = member;
    let mut extra_initialized: Option<Vec<Value>> = None;
    if ref_kind == 8 {
        let uninitialized = match ctx.get_field_by_name(this, "instanceClass") {
            Value::Object(Some(inst_mirror)) => ctx
                .class_id_from_mirror(inst_mirror)
                .filter(|&cid| !ctx.is_class_initialized(cid)),
            _ => None,
        };
        if let Some(cid) = uninitialized {
            let (pin_base, handles) = pin_mh_args(ctx, extra);
            let this_pin = ctx.pin_native_root(this);
            let member_pin = ctx.pin_native_root(member);
            let init = dmh_initialize_served_cold(ctx, cid, DmhInitBarrier::Constructor);
            this = ctx.read_native_pin(this_pin, this);
            member = ctx.read_native_pin(member_pin, member);
            extra_initialized = Some(read_pinned_mh_args(ctx, &handles, extra));
            ctx.unpin_native_roots(this_pin);
            if pin_base != usize::MAX {
                ctx.unpin_native_roots(pin_base);
            }
            if let Err(e) = init {
                return Some(Err(e));
            }
        }
    }
    let extra: &[Value] = extra_initialized.as_deref().unwrap_or(extra);
    let member_type = match ctx.get_field_by_name(member, "type") {
        Value::Object(Some(t)) => t,
        _ => return None,
    };
    if (1..=4).contains(&ref_kind) {
        let volatile = flags & FIELD_ACC_VOLATILE != 0;
        return Some(mh_real_field_access(
            ctx,
            this,
            member_type,
            ref_kind,
            volatile,
            extra,
            raw_return,
        ));
    }
    if !(5..=9).contains(&ref_kind) {
        return None;
    }
    let desc = crate::link_to_method_descriptor(&*ctx, member_type)?;
    let (params, _ret) = split_descriptor_params(&desc)?;
    let takes_receiver = ref_kind != 6;
    let mut call: Vec<Value> = Vec::with_capacity(extra.len() + 2);
    let mut allocated = None;
    if ref_kind == 8 {
        let clazz = match ctx.get_field_by_name(member, "clazz") {
            Value::Object(Some(m)) => m,
            _ => return None,
        };
        // SBR3 lang: an allocator handle (`DirectMethodHandle$Constructor`) allocates its
        // `instanceClass`, which is NOT the member's declaring class for the serialization
        // constructor (`ReflectionFactory.newConstructorForSerialization`: the member is the
        // ancestor's `<init>`, usually `Object()`, and `instanceClass` the class being
        // deserialized). Allocating `member.clazz` handed back a bare `Object` once a caller
        // was compiled (RJitMultiArrayClass s20 under the lever; cold callers run the JDK's own
        // `LambdaForm`, whose `allocateInstance` reads `instanceClass`).
        let by_instance_class = match ctx.get_field_by_name(this, "instanceClass") {
            Value::Object(Some(inst_mirror)) => {
                ctx.class_id_from_mirror(inst_mirror).and_then(|cid| {
                    ctx.instantiate(
                        cid,
                        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
                    )
                    .ok()
                    .flatten()
                })
            }
            _ => None,
        };
        let obj = match by_instance_class {
            Some(obj) => obj,
            None => {
                let class_name = resolve_class_name_robust(ctx, clazz)?;
                ctx.allocate_instance(&class_name)?
            }
        };
        // Interpreter round i2 wave 63, lane L1: `DirectMethodHandle.allocateInstance`'s
        // `Unsafe.allocateInstance`, a JVMTI `VMObjectAlloc` (MEASURED: a
        // reflective constructor posts one).
        let obj = ctx.jvmti_vm_object_alloc(obj);
        allocated = Some(obj);
        call.push(Value::Object(Some(obj)));
    }
    let lead = usize::from(takes_receiver && ref_kind != 8);
    for (i, v) in extra.iter().enumerate() {
        let v = if i >= lead {
            match params.get(i - lead).map(String::as_str) {
                Some(p) if p.len() == 1 && matches!(v, Value::Object(_)) => {
                    match mh_unbox_for_primitive(ctx, p, *v, None) {
                        Ok(u) => u,
                        Err(e) => return Some(Err(e)),
                    }
                }
                _ => *v,
            }
        } else {
            *v
        };
        call.push(v);
    }
    call.push(Value::Object(Some(member)));
    let result = crate::native_method_handle_link_to(ctx, &call);
    Some(match allocated {
        Some(obj) => result.map(|_| Some(Value::Object(Some(obj)))),
        None if raw_return => result,
        None => auto_box_return(ctx, result, &desc),
    })
}

/// The leaf descriptor (`MH_DESC`) an adapter carrier (`insertArguments`,
/// `asCollector`, `asSpreader`) records for its `target`: the target's own
/// `MH_DESC`, or, for a real JDK handle (none of this model's slots), its
/// `type` (`CRATONVM_MH_FOREIGN_VMENTRY_FIRST`). A real target left the
/// carrier with an empty descriptor, so its result went back unboxed into an
/// `Object` call site (`(Object) collector.invoke(1, 2)` answered `null`;
/// interpreter round i2 wave 55, lane L4, `L4W55RealJdkAdapters` row
/// `avc-invoke-object`).
pub(crate) fn mh_carrier_leaf_desc(ctx: &mut dyn NativeContext, target: ObjectRef) -> String {
    // A constructor target's adapter carries its `type` descriptor
    // (`mh_adapter_desc`, dev); a real JDK handle is narrower than this
    // model and has no `MH_KIND` to read.
    let own = if ctx.object_num_fields(target) > MH_KIND {
        mh_adapter_desc(ctx, target)
    } else {
        mh_read_desc(ctx, target)
    };
    if let Some(desc) = own {
        return desc;
    }
    if crate::nbflags().mh_foreign_vmentry_first && ctx.object_num_fields(target) <= MH_BOUND {
        if let Some(own) = mh_foreign_door_type(ctx, target) {
            return own;
        }
    }
    String::new()
}

/// Does this real `DirectMethodHandle`'s linker check its receiver before its
/// `linkTo*` (`DirectMethodHandle.makePreparedLambdaForm`'s
/// `needsReceiverCheck`)? `$Interface` always (`LF_INVINTERFACE`), `$Special`
/// when its `caller` is an interface (`LF_INVSPECIAL_IFC`). Reads only.
/// Interpreter round i2 wave 55, lane L4.
pub(crate) fn mh_linker_checks_receiver(
    ctx: &mut dyn NativeContext,
    dmh: ObjectRef,
    class_name: &str,
) -> bool {
    match class_name {
        "java/lang/invoke/DirectMethodHandle$Interface" => true,
        "java/lang/invoke/DirectMethodHandle$Special" => match ctx.get_field_by_name(dmh, "caller")
        {
            Value::Object(Some(mirror)) => mirror_class_id(&*ctx, mirror)
                .is_some_and(|cid| (ctx.class_access_flags(cid) & 0x0200) != 0),
            _ => false,
        },
        _ => false,
    }
}

/// SBR2 wave 10 (PUREVH), opt-in `CRATONVM_PURE_BYTECODE=1`: any other REAL
/// `MethodHandle` (a `BoundMethodHandle` species, a combinator's
/// `SimpleMethodHandle`, an adapter) runs through the JDK's own `LambdaForm`
/// interpreter, `form.interpretWithArguments(handle, args...)` -- stage 2(a) of
/// `SBR2-W7-MH-methodhandle-carrier-replacement-DESIGN-20260930.md`: boxing per
/// call, correct, no class spin. Its `NamedFunction`s reach `invokeBasic` on the
/// direct handles, which [`mh_real_direct_dispatch`] serves. Arguments are boxed
/// as the basic type the form erases them to (`Z B S C I` -> `Integer`), and the
/// call site's return (`Z B S C` back from `Integer`) restores what the site asks.
pub(crate) fn mh_real_lambda_form_dispatch(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    raw_return: bool,
) -> Option<MethodCallResult> {
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    if !matches!(ctx.get_field_by_name(this, "form"), Value::Object(Some(_))) {
        return None;
    }
    if let Some(result) = mh_real_lf_entry_fast(call_site.is_some(), ctx, this, extra, raw_return) {
        return Some(result);
    }
    // Interpreter round i2 wave 55, lane L4 (`CRATONVM_MH_FOREIGN_VMENTRY_FIRST`):
    // outside `CRATONVM_PURE_BYTECODE` the `InvokerBytecodeGenerator.generate*`
    // natives are held, so `interpretWithArguments` needs a `NamedFunction`
    // invoker the held `generateNamedFunctionInvoker` fabricates as
    // `LambdaForm.NFI()void`, and `computeInvoker` raises `InternalError` on it.
    // A handle whose form was linked to real bytecode (a pregenerated
    // `*$Holder` entry) runs that entry instead (`mh_run_foreign_vmentry`,
    // wave 53), as it did before this door was ungated.
    if mh_foreign_vmentry_first(ctx, this) {
        return None;
    }
    let (ptypes, ret) = match call_site {
        Some(cs) => crate::lang_class::parse_descriptor_param_and_return(cs),
        None => (Vec::new(), String::new()),
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let val_hs: Vec<Option<cratonvm_native_api::NativeHandle>> = extra
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => Some(scope.root(*o)),
            _ => None,
        })
        .collect();
    let arr = scope.new_array(ArrayElementType::Reference, extra.len() + 1);
    let arr_h = scope.root(arr);
    let t = Value::Object(Some(scope.get(&this_h)));
    let a0 = scope.get(&arr_h);
    scope.set_array_element(a0, 0, t);
    for (i, v) in extra.iter().enumerate() {
        let v = match &val_hs[i] {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => *v,
        };
        let boxed = match v {
            Value::Int(_) => crate::lang_class::box_value_canonical(&mut *scope, v, DESC_INT),
            Value::Long(_) => crate::lang_class::box_value_canonical(&mut *scope, v, DESC_LONG),
            Value::Float(_) => crate::lang_class::box_value(&mut *scope, v, DESC_FLOAT),
            Value::Double(_) => crate::lang_class::box_value(&mut *scope, v, DESC_DOUBLE),
            other => other,
        };
        let _ = ptypes.get(i);
        let a = scope.get(&arr_h);
        scope.set_array_element(a, i + 1, boxed);
    }
    let this_cur = scope.get(&this_h);
    let Value::Object(Some(form)) = scope.get_field_by_name(this_cur, "form") else {
        return None;
    };
    let arr_cur = Value::Object(Some(scope.get(&arr_h)));
    let result = scope.invoke_virtual_bytecode_only(
        form,
        "interpretWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        &[arr_cur],
    );
    if raw_return {
        // `invokeBasic`: exact basic types, so the result is the handle's own
        // return -- raw for a primitive, as the spun caller reads it.
        let this_cur = scope.get(&this_h);
        let rdesc = match scope.get_field_by_name(this_cur, "type") {
            Value::Object(Some(mt)) => match scope.get_field_by_name(mt, "rtype") {
                Value::Object(Some(rt)) => mirror_to_descriptor(&*scope, rt).into_owned(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        return Some(match result {
            Ok(Some(Value::Object(Some(o)))) if rdesc.len() == 1 && rdesc != "V" => {
                let raw = crate::lang_class::unbox_value(&*scope, o);
                Ok(Some(raw))
            }
            Ok(_) if rdesc == "V" => Ok(None),
            other => other,
        });
    }
    Some(match (result, ret.as_str()) {
        (Ok(Some(Value::Object(Some(o)))), "Z" | "B" | "S" | "C") => {
            let raw = crate::lang_class::unbox_value(&*scope, o);
            Ok(Some(crate::lang_class::box_value_canonical(
                &mut *scope,
                raw,
                &ret,
            )))
        }
        (Ok(_), "V") => Ok(Some(Value::Object(None))),
        (other, _) => other,
    })
}

/// The static field a real `DirectMethodHandle$StaticAccessor` names: its
/// `staticOffset` resolved against its own `staticBase`, the declaring class's
/// mirror (gci cl48, page
/// `gci-i47w-jw1-compatible-child-first-loader-statics-read-back-wrong-FIXED-20261017`).
/// The offset is minted per class NAME, so the handles of the child-first
/// copies of one class carry one offset; resolved alone it named the copy
/// registered last, and `Field.get` / `Field.set` of every copy under
/// `--compatible` shared that copy's static.
pub(crate) fn real_static_accessor_target(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    offset: usize,
) -> Option<(ClassId, usize)> {
    let base = match ctx.get_field_by_name(this, "staticBase") {
        Value::Object(base) => base,
        _ => None,
    };
    crate::unsafe_natives_ext::unsafe_static_field_target_via_base(&*ctx, base, offset)
}

/// The field kinds of [`mh_real_direct_dispatch`] (`REF_getField` ..
/// `REF_putStatic`): `member_type` is the field's `Class`.
pub(crate) fn mh_real_field_access(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    member_type: ObjectRef,
    ref_kind: i32,
    volatile: bool,
    extra: &[Value],
    raw_return: bool,
) -> MethodCallResult {
    // `Accessor.fieldOffset` / `StaticAccessor.staticOffset`.
    let offset_field = if ref_kind == 2 || ref_kind == 4 {
        "staticOffset"
    } else {
        "fieldOffset"
    };
    let offset = match ctx.get_field_by_name(this, offset_field) {
        Value::Long(v) if v >= 0 => v as usize,
        Value::Int(v) if v >= 0 => v as usize,
        _ => {
            return Err(RuntimeError::UnsupportedOperationException {
                message: {
                    let cid = ctx.class_id_of_object(this);
                    format!(
                        "real field MethodHandle without a {offset_field} ({}: {:?})",
                        ctx.class_name_of_id(cid).unwrap_or_default(),
                        ctx.get_field_by_name(this, offset_field)
                    )
                },
            }
            .into())
        }
    };
    let fdesc = mirror_to_descriptor(ctx, member_type).into_owned();
    let is_static = ref_kind == 2 || ref_kind == 4;
    // An instance kind has no static target and a static kind no receiver
    // check, so looking the target up before the receiver check changes no
    // answer (wave 73, lane L4: the split for `mh_real_field_record_fast`).
    let target = if is_static {
        // gci cl48: against the handle's own `staticBase`, as the JDK's
        // `UNSAFE.getReference(staticBase, staticOffset)` reads it.
        let target = real_static_accessor_target(ctx, this, offset).ok_or_else(|| {
            MethodCallFailed::from(RuntimeError::UnsupportedOperationException {
                message: "real static field MethodHandle: unregistered offset".to_string(),
            })
        })?;
        Some(target)
    } else {
        None
    };
    // JIT round 20 wave 8c: an instance `fieldOffset` back to its slot (a
    // byte offset under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`; an accessor
    // never names the inside of a field).
    let offset = if is_static {
        offset
    } else {
        crate::unsafe_natives_ext::unsafe_decode_field_offset(
            ctx.unsafe_byte_field_offsets(),
            offset,
        )
        .0
    };
    mh_real_field_access_resolved(
        ctx, ref_kind, offset, &fdesc, target, volatile, extra, raw_return,
    )
}

/// The access half of [`mh_real_field_access`], from a decoded shape (the
/// ordinary path's per-call decode, or [`mh_real_field_record_fast`]'s record).
///
/// `volatile` (interpreter round i3 wave 79, lane L4; page
/// `i71-L4-field-setter-handle-on-a-volatile-field-stores-without-the-volatile-fence`):
/// the member is `volatile`, so the access is the one HotSpot's accessor form
/// makes for it (`putXxxVolatile` / `getXxxVolatile`): the store is fenced as
/// `VarHandle.setVolatile` fences it ([`field_handle_volatile_store`]) and the
/// load is followed by an acquire fence ([`field_handle_volatile_load_fence`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn mh_real_field_access_resolved(
    ctx: &mut dyn NativeContext,
    ref_kind: i32,
    offset: usize,
    fdesc: &str,
    target: Option<(ClassId, usize)>,
    volatile: bool,
    extra: &[Value],
    raw_return: bool,
) -> MethodCallResult {
    let is_static = ref_kind == 2 || ref_kind == 4;
    let is_put = ref_kind == 3 || ref_kind == 4;
    let lead = usize::from(!is_static);
    let receiver = if is_static {
        None
    } else {
        match extra.first() {
            Some(Value::Object(Some(r))) => Some(*r),
            _ => {
                // `checkBase`'s `Objects.requireNonNull`, listed under its
                // two frames as the ordinary field arms list it (interpreter
                // round i2 wave 62, lane L3; `L3W56FieldHandleNullBase`,
                // `L3W54FieldOverrideAccessorFrames` under `--jdk-only`).
                note_field_handle_check_base_chain(ctx);
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
        }
    };
    // Interpreter round i2 wave 62, lane L3: the static field's class is
    // initialized first, as `DirectMethodHandle$StaticAccessor`'s barrier
    // does (`ensureInitialized`), listing its frames when the `<clinit>`
    // fails (`L4W59HandleClassInitFrames` `mh-static-getter` / `-setter`
    // under `--jdk-only`: the getter answered 0, the setter wrote). The
    // arguments are re-read after the `<clinit>` (a GC point).
    let refreshed: Vec<Value>;
    let extra = match target {
        Some((class_id, _)) if !ctx.is_class_initialized(class_id) => {
            let mut copy = extra.to_vec();
            let (base, handles) = pin_mh_args(ctx, &copy);
            let init = dmh_initialize_served_cold(ctx, class_id, DmhInitBarrier::StaticField);
            vh_refresh_live(ctx, base, &handles, &mut copy);
            init?;
            refreshed = copy;
            &refreshed[..]
        }
        _ => extra,
    };
    // The value a put stores, read before the initialization below (a GC
    // point) and re-read through its pin after it.
    let mut put_value = if is_put {
        extra.get(lead).copied()
    } else {
        None
    };
    // `StaticAccessor`'s barrier (`DirectMethodHandle.ensureInitialized`): the
    // field's class is initialized before the first access, as HotSpot's
    // `LF_*_CINIT` forms do. This lane read and wrote the storage of an
    // uninitialized class: `findStaticGetter(C, "X", int).invokeExact()` read
    // `0` where HotSpot runs `C.<clinit>` and reads its value, and a setter's
    // store was overwritten by the `<clinit>` that ran later (interpreter round
    // i2 wave 62, lane L4; `L4W62InheritedStaticFieldHandles`, `--jdk-only`).
    // An initialized class pays one state read. Kill switch
    // `CRATONVM_MH_REAL_STATIC_FIELD_INITIALIZES=0`.
    if let Some((class_id, _)) = target {
        if crate::nbflags().mh_real_static_field_initializes && !ctx.is_class_initialized(class_id)
        {
            let value_pin = pin_mh_value(ctx, put_value.unwrap_or(Value::Int(0)));
            let initialized = dmh_initialize_served(ctx, class_id, DmhInitBarrier::StaticField);
            if let Some(v) = put_value {
                put_value = Some(read_pinned_mh_arg(ctx, value_pin, v));
            }
            if value_pin != usize::MAX {
                ctx.unpin_native_roots(value_pin);
            }
            initialized?;
        }
    }
    if is_put {
        let mut v = put_value.unwrap_or(Value::Int(0));
        if fdesc.len() == 1 && matches!(v, Value::Object(_)) {
            v = mh_unbox_for_primitive(ctx, fdesc, v, None)?;
        }
        field_handle_volatile_store(volatile, || match (receiver, target) {
            (Some(r), _) => ctx.set_field(r, offset, v),
            (None, Some((c, i))) => ctx.set_static_field(c, i, v),
            _ => {}
        });
        return Ok(if raw_return {
            None
        } else {
            Some(Value::Object(None))
        });
    }
    let v = match (receiver, target) {
        (Some(r), _) => ctx.get_field(r, offset),
        (None, Some((c, i))) => ctx.get_static_field(c, i),
        _ => Value::Object(None),
    };
    field_handle_volatile_load_fence(volatile);
    if fdesc.len() == 1 && !raw_return {
        Ok(Some(crate::lang_class::box_value_canonical(ctx, v, fdesc)))
    } else {
        Ok(Some(v))
    }
}

/// `MethodHandle.invokeBasic(...)` on one of CratonVM's synthetic handles, or
/// `None` when the receiver is not one (the caller keeps its old answer).
///
/// `invokeBasic` is the JDK-internal, unchecked invocation (basic types, no
/// `asType`), and its registration (`lib.rs::native_method_handle_invoke`)
/// answered `null` for every handle but a Panama downcall. The JDK calls it
/// itself: `StringConcatFactory`'s hidden-class strategy builds its
/// concatenator's receiver with `constructor.invokeBasic((Object) constants)`,
/// so every concatenation of two or more arguments spun as Java (direct
/// `StringConcatFactory` calls, `ObjectMethods.makeToString`) bound its
/// concatenator to `null` and then answered `null` (interpreter round i1
/// wave 43, lane L4; host run of `L4W43CopyWithViews`, rows `concat-consts`,
/// `om-tostring`, ...). No call-site type is armed for `invokeBasic`, so the
/// dispatch takes the door's permissive argument path, as `invoke` does with
/// no call site; the cell is taken anyway so no stale descriptor survives.
pub(crate) fn mh_invoke_basic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    if ctx.object_num_fields(this) <= MH_BOUND {
        if let Some(result) = mh_real_lf_entry_fast(false, ctx, this, &args[1..], true) {
            return Some(result);
        }
    }
    if let Some(result) = mh_real_direct_fast(ctx, this, &args[1..], None, true, false) {
        return Some(result);
    }
    // Wave 73 (`CRATONVM_MH_REAL_FIELD_RECORD`): a real field accessor.
    if let Some(result) = mh_real_field_record_fast(ctx, this, &args[1..], None, true) {
        return Some(result);
    }
    // SBR5 lane avh: a `BoundMethodHandle` species (what every adapter and combinator of the
    // real JDK is) is neither a delegating handle nor a direct one: its form's entry runs
    // without the delegation walk and the direct decode below (a by-name class lookup and a
    // class-name `String` per crossing).
    if ctx.object_num_fields(this) <= MH_BOUND {
        let cid = ctx.class_id_of_object(this);
        if ctx
            .class_name_arc_of_id(cid)
            .is_some_and(|n| n.starts_with(BMH_SPECIES_PREFIX))
        {
            if let Some(result) = mh_real_lambda_form_dispatch(ctx, this, &args[1..], None, true) {
                return Some(result);
            }
        }
    }
    let this = mh_door_handle(ctx, this);
    if let Some(result) = mh_real_direct_dispatch(ctx, this, &args[1..], true) {
        return Some(result);
    }
    if let Some(result) = mh_real_lambda_form_dispatch(ctx, this, &args[1..], None, true) {
        return Some(result);
    }
    // A real JDK handle (interpreter round i2 wave 54, lane L4): a
    // `LambdaForm` this VM runs -- `Invokers.basicInvoker`'s, a
    // `BoundMethodHandle` species' -- calls `invokeBasic` on it, which ran its
    // form's `vmentry` on HotSpot and answered `null` here (the caller's
    // fallback below). Its arguments are its own basic types, its result raw.
    if mh_foreign_door_type(ctx, this).is_some() {
        return Some(match mh_run_foreign_vmentry(ctx, this, &args[1..], None) {
            Some(answer) => answer,
            None if crate::nbflags().mh_foreign_handle_raises => {
                Err(mh_foreign_handle_refusal(ctx, this))
            }
            None => return None,
        });
    }
    // Width-guarded, as `MH_VARARGS` asks: only a handle with this model's
    // slots, and a kind in them.
    if ctx.object_num_fields(this) <= MH_BOUND
        || !matches!(ctx.get_field(this, MH_KIND), Value::Int(_))
    {
        return None;
    }
    let _ = cratonvm_native_api::poly_call_site::take();
    Some(mh_invoke_door(ctx, this, &args[1..], None, false))
}

/// The body of the `MethodHandle.invoke` door: `this.asType(callSiteType)`
/// followed by an exact invocation, `call_site` being the descriptor the
/// caller wrote (`cratonvm_native_api::poly_call_site`, taken by the door
/// before anything can re-enter the interpreter).
///
/// A function rather than the registration's closure since interpreter round
/// i1 wave 28 (lane L4), because a plain invoker is this door on its target:
/// `MethodHandles.invoker(t).invokeExact(target, args)` is
/// `target.invoke(args)` at the call-site type `t` (HotSpot's
/// `Invokers.checkGenericType`, an `asType(t)`), which the `MH_KIND_INVOKER`
/// arm of [`mh_dispatch`] now runs through here. `tail_ok` is the caller's
/// own [`MH_TAIL`]: `false` when something between the outermost door and
/// here does work after this returns, which a transparent leaf would let a
/// yield skip; the door itself passes `true`. (The invoker arm passes `true`
/// only for a reference target return under a reference token: it unboxes a
/// primitive one after the call.)
pub(crate) fn mh_invoke_door(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    tail_ok: bool,
) -> MethodCallResult {
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_INVOKE_CALLSITE] {call_site:?}");
    }
    // The invoker arms enter here with the handle they were given (wave 45).
    let this = mh_door_handle(ctx, this);
    // JIT round 16 wave 11 (lane mhbind): a downcall carrier is a STATIC
    // handle of its own `type` here (`mh_downcall_door_type`).
    let downcall_type = mh_downcall_door_type(ctx, this);
    // Interpreter round i2 wave 54 (lane L4): a real JDK handle is a STATIC
    // handle of its own `type` here too (`mh_foreign_door_type`); the
    // dispatch converts its arguments (`mh_run_foreign_vmentry`).
    let foreign_type = match &downcall_type {
        Some(_) => None,
        None => mh_foreign_door_type(ctx, this),
    };
    let desc = match (&downcall_type, &foreign_type) {
        (Some(t), _) | (None, Some(t)) => t.clone(),
        (None, None) => mh_read_desc(ctx, this).unwrap_or_default(),
    };
    // A real JDK handle (`foreign_type`) has none of this model's slots: its
    // `MH_KIND` / `MH_BOUND` are not read (a heap OOB warning per call;
    // interpreter round i2 wave 56, lane L4).
    let kind = if downcall_type.is_some() || foreign_type.is_some() {
        MH_KIND_STATIC
    } else {
        mh_kind_or_virtual(&*ctx, this)
    };
    // For virtual/special handles the first extra arg is the RECEIVER, not
    // a descriptor param — adapting it against param_types[0] would misalign
    // every param by one and (now that adapt unboxes) could wrongly unbox a
    // wrapper receiver. Skip the receiver, then adapt the params 1:1.
    let needs_receiver = kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL;
    let has_bound = foreign_type.is_none() && mh_has_bound_guarded(&*ctx, this);
    // Round 15 wave 4 (lane mh2): `invoke` is `asType(callSiteType)` (its
    // `WrongMethodTypeException`), then the adapter's argument casts (its
    // `ClassCastException`), and only then the receiver's null check in the
    // direct handle. See [`mh_invoke_null_receiver_after_casts_enabled`].
    let null_receiver_first = !mh_invoke_null_receiver_after_casts_enabled();
    if null_receiver_first {
        if let Some(refusal) = null_receiver_refusal(ctx, this, kind, extra) {
            return Err(refusal);
        }
    }
    // Read ONCE, while `this` is current (round 12 wave 7): the narrowing
    // refusal, the declared cast, an adapter's entry conversion and the
    // return cast after the dispatch all need the declared type, and each
    // used to render the `MethodType` again (a mirror-to-name resolution and
    // a `String` per parameter). Also whether it is an
    // `explicitCastArguments` stamp.
    let declared = mh_declared_descriptor(ctx, this);
    // One read of the marking bits (wave 58): the explicit stamp, and whether
    // a stamp judges the return itself (`invoke_return_unbox`).
    let marks = mh_marking_bits(&*ctx, this);
    let explicit_stamp = marks & MH_MARK_EXPLICIT_CAST != 0;
    if let Some(site) = call_site.as_deref() {
        // Round 15 wave 4 (lane mh3): a bound handle is fixed-arity, so a site
        // of another arity is `asType`'s refusal, before any conversion.
        if let Some(refusal) = bound_handle_arity_refusal(ctx, this, site, declared.as_deref()) {
            return Err(refusal);
        }
        // Wave 54 (lane L4): the kinds judged for `invokeExact` only.
        if let Some(refusal) =
            invoke_exact_typed_kind_arity_refusal(ctx, this, kind, site, declared.as_deref())
        {
            return Err(refusal);
        }
        if let Some(refusal) =
            invoke_narrowing_arg_refusal(ctx, this, kind, site, declared.as_deref())
        {
            return Err(refusal);
        }
    }
    // Before `adapt_invoke_args`, which unboxes and can re-enter the
    // interpreter: a wrong reference must fail as the cast `asType`
    // performs, not as whatever the callee does with it.
    if let Some(refusal) = invoke_reference_cast_refusal_with(
        ctx,
        this,
        kind,
        needs_receiver,
        has_bound,
        extra,
        call_site.as_deref(),
        declared.as_deref(),
    ) {
        return Err(refusal);
    }
    if !null_receiver_first {
        if let Some(refusal) = null_receiver_refusal(ctx, this, kind, extra) {
            return Err(refusal);
        }
    }
    // An INVOKER passes its arguments as passed too (wave 27): its
    // `MH_DESC` is the TARGET's descriptor (`mh_alloc_invoker`), one
    // parameter short of the invoker's own arguments (the target
    // handle leads them), and the target's own leaf converts them.
    let adapted = if kind == MH_KIND_ARRAY_GET
        || kind == MH_KIND_ARRAY_SET
        || kind == MH_KIND_INVOKER
        // A real JDK handle's dispatch converts its arguments with the call
        // site's static types (`mh_run_foreign_vmentry`), as this door armed.
        || foreign_type.is_some()
    {
        // The array accessor arm of `mh_dispatch` converts its own
        // index and value (`asType`'s reference -> primitive rules,
        // with the call site's static types; wave 25), and it must
        // see them AS PASSED. `adapt_invoke_args` unboxed them first
        // against `MH_DESC`, so the arm met a raw primitive and only
        // widened it: `arrayElementSetter(boolean[].class)
        // .invoke(arr, 0, (Object) Byte.valueOf(1))` stored `true`
        // where HotSpot throws `Cannot cast java.lang.Byte to
        // java.lang.Boolean`, and a `Long` into a `byte[]` stored 0
        // (`L4/L4W25PrimitiveArraySetter`, 40 of 113 lines on the
        // merged wave-25 build). `adapt_single_arg` is also aligned
        // against the unbound descriptor, one slot off for a handle
        // bound to its array.
        extra.to_vec()
    } else if (kind_has_authoritative_adapted_type(kind)
        && !kind_has_authoritative_type(kind))
        // Round 15 wave 1 (lane mh): `MH_MARK_EXACT_INVOKER` named explicitly.
        // It WAS `MH_MARK_CAST_CHAIN`'s bit until the two were split, so an
        // exact invoker has always taken this branch; the split keeps that.
        || mh_marking_bits(&*ctx, this)
            & (MH_MARK_EXPLICIT_CAST | MH_MARK_CAST_CHAIN | MH_MARK_EXACT_INVOKER)
            != 0
    {
        // An ADAPTER: convert to its declared (`type`) parameters, not
        // to `MH_DESC`, which is its leaf's and misaligned (wave 26;
        // see `adapter_entry_args`). A reference that does not convert
        // is refused here, before the adapter's own work runs. A handle
        // `explicitCastArguments` stamped takes the same `asType` step
        // to its stamp; the leaf then casts from the stamp
        // (`MH_EXPLICIT_SCOPE`), not from `MH_DESC`. So does a stamp
        // over a stamp (round 12 wave 7): its chain is judged from the
        // outer type (`MH_CAST_CHAIN`), so the arguments must reach
        // the dispatch typed by it, not already unboxed for the leaf.
        adapter_entry_args(ctx, declared.as_deref(), extra, call_site.as_deref())?
    } else if needs_receiver && !has_bound && !extra.is_empty() {
        let mut v = Vec::with_capacity(extra.len());
        v.push(extra[0]);
        v.extend(adapt_invoke_args(ctx, &extra[1..], &desc));
        v
    } else {
        adapt_invoke_args(ctx, extra, &desc)
    };
    // The one door where the answer genuinely varies with what the
    // caller WROTE: `mh.invoke((String[]) null)` passes the null
    // through and `mh.invoke((Object) null)` collects it into a
    // one-element array. `vm_exec`'s signature-polymorphic dispatch
    // publishes the descriptor; a miss simply leaves the permissive
    // path in charge, which is the pre-2026-08-21 behaviour.
    //
    // A virtual or special call site names the receiver and `MH_DESC`
    // does not, so strip it here rather than teach every comparison
    // downstream about the difference.
    // Kept before the receiver strip below consumes `call_site`: the
    // RETURN type is the same in both spellings, and the conversion
    // after dispatch needs it.
    let call_site_ret = call_site;
    if let Some(cs) = call_site {
        let stripped = if needs_receiver && !has_bound {
            descriptor_without_first_param(cs)
        } else {
            None
        };
        arm_entry_call_site(stripped.as_deref().unwrap_or(cs));
    }
    // A reference call-site return is handed back unchanged when the
    // leaf returns a reference too, so the door may be transparent to
    // a virtual thread's yield beneath it (`MH_TAIL`, wave 26) -- unless
    // the return cast below still has work to do after the dispatch, or
    // the caller has (`tail_ok`).
    // Since wave 28 the token is the call site's return (`door_tail_token`):
    // the door's work after the dispatch is converting the leaf's result to
    // it, which a remounted leaf frame can do instead
    // (`tail_leaf_conversion`).
    // One plan for every door (wave 59, lane L4; `DoorReturn`), built before
    // the dispatch: its tail token too (wave 60).
    let plan = DoorReturn::invoke(
        kind,
        &desc,
        declared.as_deref(),
        call_site_ret,
        marks,
        explicit_stamp,
    );
    let tail = if tail_ok && downcall_type.is_none() && foreign_type.is_none() {
        plan.invoke_tail()
    } else {
        0
    };
    let result = mh_dispatch_from_door(ctx, this, &adapted, tail);
    // Wave 54 (lane L4): a leaf's arity refusal names this call site.
    let result = result.map_err(|failed| {
        mh_deep_refusal_site_text(ctx, failed, declared.as_deref(), call_site_ret, false)
    });
    // `invoke` is specified as `asType(callSiteType)` followed by an exact
    // invocation, so the CALL SITE names the return type the caller gets:
    // its reference cast, its unboxing (wave 58) and its widening
    // (`(long) findStatic(Math,"max",(int,int)int).invoke(1, 2)` is `2L`),
    // except a reference site over a primitive declared return the leaf does
    // not share, which boxes the declared type's value (round 13 wave 3).
    door_return_apply(ctx, result, &plan)
}

/// `CRATONVM_MH_INVOKE_NULL_RECEIVER_AFTER_CASTS` -- default ON (round 15 wave
/// 4, lane mh2; `r15w4-mh2-invoke-null-receiver-before-astype-FIXED-20260930.md`);
/// `0`/`false`/`off`/`no` restores the `invoke` door's old order, where a
/// `null` receiver of an unbound virtual/special handle was refused
/// ([`null_receiver_refusal`]) before the call site's
/// `WrongMethodTypeException` ([`invoke_narrowing_arg_refusal`]) and the
/// argument casts' `ClassCastException`
/// ([`invoke_reference_cast_refusal_with`]). HotSpot's `invoke` runs
/// `asType(callSiteType)` first and the adapter's casts next; the direct
/// handle's receiver check is last, so `(Mid,int)int.invoke((Mid) null, 5L)`
/// is a `WrongMethodTypeException` and `(Mid,Sub)int.invoke((Mid) null,
/// (Object) "x")` a `ClassCastException` there, not an NPE. Spec
/// conformance, both modes. Read once.
pub(crate) fn mh_invoke_null_receiver_after_casts_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_MH_INVOKE_NULL_RECEIVER_AFTER_CASTS",
        )
    })
}

/// Should the `invoke` door box a result against the handle's DECLARED return
/// rather than the call site's? Only for a reference call site over a
/// primitive declared return that differs from the leaf's primitive return
/// (the one shape where the call-site conversion has nothing to widen and
/// used to box the leaf's value). Kill switch
/// `CRATONVM_MH_INVOKE_BOXES_DECLARED_RETURN` (default on, both modes; round
/// 13 wave 3, lane ffm2).
pub(crate) fn invoke_boxes_declared_return(
    leaf_desc: &str,
    declared: Option<&str>,
    call_site: Option<&str>,
) -> bool {
    let Some(site) = call_site else {
        return false;
    };
    return_type_desc(site).starts_with('L') && boxes_declared_return(leaf_desc, declared)
}

/// The `invokeWithArguments` door's half of [`invoke_boxes_declared_return`]:
/// its call site is always `Object`, so only the declared-vs-leaf test
/// remains. Round 13 wave 3 hand-back (lane ffm2): both overloads boxed a
/// primitive result against the LEAF's return (`auto_box_return(.., desc)`),
/// so `retLong.asType(()double).invokeWithArguments()` answered a `Long`
/// (and a cast chain's `(double)(int) r` came back as the integral box)
/// where HotSpot answers a `Double`.
pub(crate) fn invoke_with_arguments_boxes_declared_return(leaf_desc: &str, declared: Option<&str>) -> bool {
    boxes_declared_return(leaf_desc, declared)
}

/// A primitive declared return that differs from the leaf's primitive return,
/// with `CRATONVM_MH_INVOKE_BOXES_DECLARED_RETURN` on (default on, both
/// modes; round 13 wave 3, lane ffm2). The switch covers both the `invoke`
/// and the `invokeWithArguments` door.
pub(crate) fn boxes_declared_return(leaf_desc: &str, declared: Option<&str>) -> bool {
    let Some(declared) = declared else {
        return false;
    };
    let leaf_ret = return_type_desc(leaf_desc);
    let declared_ret = return_type_desc(declared);
    if !(static_primitive_desc(leaf_ret).is_some()
        && static_primitive_desc(declared_ret).is_some()
        && declared_ret != leaf_ret)
    {
        return false;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_BOXES_DECLARED_RETURN")
    })
}

/// The body of the `MethodHandle.invokeExact` door, `call_site` being the
/// descriptor the caller wrote.
///
/// A function rather than the registration's closure since interpreter round
/// i1 wave 28 (lane L4), because an exact invoker is this door on its target:
/// `MethodHandles.exactInvoker(t).invokeExact(target, args)` is
/// `target.invokeExact(args)` at the call-site type `t` (HotSpot's
/// `Invokers.checkExactType`), which the `MH_KIND_INVOKER` arm of
/// [`mh_dispatch`] now runs through here, so a target whose type is not `t`
/// throws the `WrongMethodTypeException` HotSpot throws. `tail_ok` as for
/// [`mh_invoke_door`].
pub(crate) fn mh_invoke_exact_door(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    tail_ok: bool,
) -> MethodCallResult {
    // The exact invoker arm enters here with the handle it was given (wave 45).
    let this = mh_door_handle(ctx, this);
    // Interpreter round i2 wave 54 (lane L4): a real JDK handle is a STATIC
    // handle of its own `type` (`mh_foreign_door_type`): no receiver of this
    // model's to refuse a `null` first argument as, and the result boxed from
    // that type's return.
    let foreign_type = mh_foreign_door_type(ctx, this);
    let desc = match &foreign_type {
        Some(t) => t.clone(),
        None => mh_read_desc(ctx, this).unwrap_or_default(),
    };
    // The historical NOTE here said a strict `invokeExact` check
    // "cannot be enforced on CratonVM's synthetic `MH_KIND_*` model",
    // because adapters keep their inner target's descriptor and a
    // strict ARITY check aborted the VM mid-dispatch (Groovy's
    // `IndyInterface` dispatches its chains through `invokeExact`:
    // "internal error: WrongMethodTypeException: expected 2 args, got
    // 1"). That is true of `MH_DESC`, which is what such a check would
    // have read. It is not true of `type`, the adapted MethodType every
    // combinator in this file already chains off. See
    // `exact_call_site_refusal` for the check, and
    // `kind_has_authoritative_adapted_type` for exactly which kinds it
    // is allowed to speak about -- the `MethodHandles` combinator
    // surface is in, MEASURED against Groovy/Netty/Spring
    // (the retired L5-residuals write-up); `TRY_FINALLY` and the
    // `LOOP_*` family stay out, deliberately, still unmeasured.
    // A real JDK handle's absent `MH_KIND` is not read (wave 56, lane L4).
    let kind = if foreign_type.is_some() {
        MH_KIND_STATIC
    } else {
        mh_kind_or_virtual(&*ctx, this)
    };
    // A retyped handle (`asType`) dispatches against the LEAF
    // descriptor and must return the DECLARED one -- see
    // `box_return_against_target`. Read once (round 12 wave 7): the
    // exact-signature check reads it too.
    let declared = mh_declared_descriptor(ctx, this);
    // One read of the marking bits (wave 58): the explicit stamp, and whether
    // a stamp judges the return itself (`invoke_return_unbox`).
    let marks = mh_marking_bits(&*ctx, this);
    let explicit_stamp = marks & MH_MARK_EXPLICIT_CAST != 0;
    if let Some(site) = call_site.as_deref() {
        if let Some(refusal) = exact_call_site_refusal(ctx, this, kind, site, declared.as_deref()) {
            return Err(refusal);
        }
    }
    if let Some(refusal) = null_receiver_refusal(ctx, this, kind, extra) {
        return Err(refusal);
    }
    if let Some(refusal) =
        invoke_exact_inner_cast_refusal(ctx, this, kind, declared.as_deref(), extra)
    {
        return Err(refusal);
    }
    // The interpreter unboxes the result against the CALL SITE after
    // this native returns; only a reference site leaves it unchanged
    // (`MH_TAIL`, wave 26) -- and only when the return cast below has
    // nothing to do after the dispatch (nor the caller, `tail_ok`).
    // The token is the call site's return when the declared type's agrees
    // (wave 28; see `mh_invoke_door`): the door converts the leaf's result to
    // the declared return, the interpreter then to the call site's.
    // The declared type is the call site's (wave 58: an `asType` copy's
    // reference leaf return at its primitive declared one; wave 59: one plan,
    // which decides the tail token too since wave 60).
    let plan = DoorReturn::exact(kind, &desc, declared.as_deref(), marks, explicit_stamp);
    let tail = if tail_ok {
        plan.exact_tail(call_site.as_deref())
    } else {
        0
    };
    let result = mh_dispatch_from_door(ctx, this, extra, tail);
    // Wave 54 (lane L4): a leaf's arity refusal names this call site.
    let result = result.map_err(|failed| {
        mh_deep_refusal_site_text(ctx, failed, declared.as_deref(), call_site, true)
    });
    door_return_apply(ctx, result, &plan)
}

