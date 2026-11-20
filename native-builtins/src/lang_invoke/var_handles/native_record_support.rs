#[allow(unused_imports)]
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
pub fn native_record_support_deserialization_ctr(
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
pub fn record_deser_dispatch(
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
        // `L8W98RecordStreamShapes.java`.
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
pub fn record_decode_primitive(
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
pub fn default_for_descriptor(desc: &str) -> Value {
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
pub fn widen_descriptor(
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
pub fn count_descriptor_params(desc: &str) -> usize {
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
pub fn pin_mh_args(ctx: &mut dyn NativeContext, args: &[Value]) -> (usize, Vec<usize>) {
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
pub fn read_pinned_mh_arg(ctx: &dyn NativeContext, handle: usize, arg: Value) -> Value {
    match arg {
        Value::Object(Some(obj)) if handle != usize::MAX => {
            Value::Object(Some(ctx.read_native_pin(handle, obj)))
        }
        _ => arg,
    }
}

/// gen r4w3/rooting: re-read a whole arg slice pinned by [`pin_mh_args`]
/// (`handles` is that call's second result), yielding current addresses.
pub fn read_pinned_mh_args(ctx: &dyn NativeContext, handles: &[usize], args: &[Value]) -> Vec<Value> {
    args.iter()
        .zip(handles.iter())
        .map(|(arg, h)| read_pinned_mh_arg(ctx, *h, *arg))
        .collect()
}

/// gen r4w3/rooting: pin one (possibly non-object) value; `usize::MAX` for a
/// non-object, matching the handle convention of [`pin_mh_args`].
pub fn pin_mh_value(ctx: &mut dyn NativeContext, v: Value) -> usize {
    match v {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    }
}

pub fn adapt_invoke_args(ctx: &mut dyn NativeContext, args: &[Value], desc: &str) -> Vec<Value> {
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
pub fn adapt_single_arg(ctx: &mut dyn NativeContext, arg: Value, expected_type: &str) -> Value {
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
                widen_primitive_to_descriptor(crate::lang_class::unbox_value(ctx, obj), DESC_LONG)            }
            other => other,
        },
        // Widening: int → float (or unbox a Float/Integer wrapper).
        "float" => match arg {
            Value::Int(v) => Value::Float(v as f32),
            Value::Long(v) => Value::Float(v as f32),
            Value::Object(Some(obj)) => {
                widen_primitive_to_descriptor(crate::lang_class::unbox_value(ctx, obj), DESC_FLOAT)            }
            other => other,
        },
        // Widening: int → double, long → double, float → double (or unbox).
        "double" => match arg {
            Value::Int(v) => Value::Double(v as f64),
            Value::Long(v) => Value::Double(v as f64),
            Value::Float(v) => Value::Double(v as f64),
            Value::Object(Some(obj)) => {
                widen_primitive_to_descriptor(crate::lang_class::unbox_value(ctx, obj), DESC_DOUBLE)            }
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
pub fn leaf_site_params(site: Option<&str>, lead: usize, total: usize) -> Option<Vec<String>> {
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
pub fn adapt_leaf_arg(
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
pub fn leaf_short_arity_refusal(
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
pub fn mh_exact_type_text(shown_declared: &str, shown_site: &str) -> String {
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
pub fn mh_deep_refusal_site_text(
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
pub fn invoke_exact_typed_kind_arity_refusal(
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
pub fn adapt_leaf_args(
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
pub fn leaf_box_raw_for_reference(
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
pub fn adapter_entry_args(
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
pub fn explicit_cast_primitive(v: Value, pdesc: &str) -> Value {
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
    pub static MH_EXPLICIT_SCOPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

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
    pub static MH_TAIL: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };

    /// The address of the `WrongMethodTypeException` a LEAF arm's arity
    /// refusal ([`leaf_short_arity_refusal`]) raised last on this thread, or
    /// `0`: a refusal that does not know the call site's type, for the door
    /// that does to reword ([`mh_deep_refusal_site_text`]; interpreter round
    /// i2 wave 54, lane L4). Compared by identity only, never dereferenced; any
    /// door's error path takes it, so a record no door claimed is dropped by
    /// the next one.
    pub static MH_DEEP_ASTYPE_REFUSAL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read and clear [`MH_TAIL`].
pub fn take_mh_tail() -> u8 {
    MH_TAIL.with(|f| f.replace(0))
}

/// Arm [`MH_TAIL`] for the dispatch made IMMEDIATELY after this call.
pub fn set_mh_tail(token: u8) {
    MH_TAIL.with(|f| f.set(token));
}

/// [`MH_TAIL`]'s token for a door whose caller receives the result as the
/// return type `ret` (a descriptor token): `b'L'` for a reference or array,
/// the primitive's (or `void`'s) descriptor byte, `0` for anything else.
pub fn door_tail_token(ret: &str) -> u8 {
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
pub fn tail_leaf_conversion(tail: u8, leaf: u8) -> Option<Option<&'static str>> {
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
pub fn tail_leaf_conversion_with(
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
pub struct TailLeafCall {
    pub _transparent: cratonvm_native_api::continuation_pin::TransparentGuard,
    pub owed: Option<(usize, &'static str)>,
    /// The leaf returns `void` behind a reference token (the door's `null`,
    /// `CRATONVM_MH_VOID_TAIL_UNMOUNT`, interpreter round i6 wave 115, lane
    /// L4): a yield is recorded through
    /// `NativeContext::note_continuation_void_leaf_null`, which also covers a
    /// native leaf that yielded itself and pushed no frame.
    pub void_leaf: bool,
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
pub fn transparent_leaf_call(
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
pub fn transparent_leaf_call_done(
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
pub fn mh_explicit_scope() -> bool {
    MH_EXPLICIT_SCOPE.with(|f| f.get())
}

/// [`mh_dispatch`] from one of the `MethodHandle` doors: the explicit-cast
/// scope of any dispatch this door was reached from (through the Java code a
/// leaf ran) is not this call's, so it is cleared for the call and restored
/// after it. `tail`: the door's [`MH_TAIL`] token ([`door_tail_token`]; `0`
/// when the door has post-call work beyond converting the leaf's return to
/// it).
pub fn mh_dispatch_from_door(
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
pub fn mh_zero_value(ty: &str) -> Value {
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
pub fn collect_trailing_varargs(
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
pub fn varargs_coerce_int(ctx: &mut dyn NativeContext, v: Value) -> i32 {
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
pub fn build_varargs_array(
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

// Extract the return type descriptor from a method descriptor.
// e.g. "(II)I" → "I", "(Ljava/lang/String;)V" → "V"
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
pub fn mh_strict_exact_signature() -> bool {
    static STRICT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STRICT.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
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
pub fn kind_has_authoritative_type(kind: i32) -> bool {
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
pub fn kind_has_authoritative_adapted_type(kind: i32) -> bool {
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
pub fn mh_loop_kinds_typed_enabled() -> bool {
    true
}

/// The handle's declared signature, read from the `type` field ONLY.
///
/// Never `mh_type_descriptor`, whose `MH_DESC` fallback would hand back a
/// virtual handle's receiver-less descriptor and make every correct call look
/// like an arity mismatch. Same rule as `bindTo`'s leading-parameter guard: no
/// `type`, or one whose mirrors do not render, means the signature is UNKNOWN,
/// and an unknown signature is an accept.
pub fn mh_declared_descriptor(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if crate::nbflags().mh_return_plan {
        return mh_return_facts_declared(ctx, mh).map(|d| String::from(&*d));
    }
    mh_declared_descriptor_rendered(ctx, mh)
}

/// [`mh_declared_descriptor`]'s render: the `type` field's `MethodType`.
pub fn mh_declared_descriptor_rendered(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
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
pub struct MhReturnFacts {
    /// [`mh_declared_descriptor_rendered`]'s answer, once asked.
    pub declared: Option<Option<std::rc::Rc<str>>>,
    /// The parsed [`MH_CAST_CHAIN`], once asked.
    pub chain: Option<Option<std::rc::Rc<CastChain>>>,
}

/// One direct-mapped line of [`MH_RETURN_FACTS_MEMO`].
pub struct MhReturnFactsLine {
    pub vm: usize,
    pub epoch: u64,
    pub mh: usize,
    pub facts: MhReturnFacts,
}

/// Slots in [`MH_RETURN_FACTS_MEMO`]; a power of two (the index is a mask).
pub const MH_RETURN_FACTS_SLOTS: usize = 64;

thread_local! {
    /// Per-thread memo of [`MhReturnFacts`], keyed by the VM, the collection
    /// epoch and the handle's address (see the section comment). A context
    /// reporting VM identity `0` (the mocks) is never memoised.
    pub static MH_RETURN_FACTS_MEMO: std::cell::RefCell<Vec<MhReturnFactsLine>> =
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
pub fn mh_return_facts<T: Clone>(
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
pub fn mh_return_facts_declared(
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
pub fn mh_return_facts_chain(
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
pub fn null_receiver_refusal(
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
pub fn mh_wmte_jdk_frames_enabled() -> bool {
    true
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
pub fn mh_door_wmte(
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
/// `L4W40IdentityConstantInvokeExact.java`): each is
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
pub fn exact_call_site_refusal(
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
    // `L4W23InvokeExactMessage.java`; `expected ...
    // but found ...` was an older JDK's (wave 23, lane L4).
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::ExactType,
        &mh_exact_type_text(&shown_declared, &shown_site),
    ))
}

/// `CRATONVM_MH_REAL_UNSPELLABLE_TYPE_ADAPTS` (default on; wave 62, lane L4):
/// see [`mh_real_invoke_adapt`]'s unspellable-type arm.
pub fn mh_real_unspellable_type_adapts() -> bool {
    crate::nbflags().mh_real_unspellable_type_adapts
}

/// `invoke` on a REAL `MethodHandle` (pure lever) is `asType(callSiteType)`
/// then an exact call: when the handle's own `type()` is not the call site's,
/// answer the adapted handle (the JDK's own `asType`, so a varargs collector
/// collects and a boxing adapter boxes). `Ok(None)` when no adaptation applies.
pub fn mh_real_invoke_adapt(
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
        // lane L4; `L4W62LambdaProxyOwnerCast`, `--real-jdk`).
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
pub fn mh_site_method_type(
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
pub fn mh_site_type_link_error(
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
    let built = cratonvm_native_api::throwable_builder::construct_new(
        ctx,
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
pub const MH_SITE_TYPE_FACTORY_DESC: &str =
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
pub fn mh_site_type_boot_loader_enabled() -> bool {
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
pub fn mh_site_type_table_enabled() -> bool {
    crate::nbflags().mh_site_type_table
}

/// Marks a site-type row of the bound-`MethodType` table (SOH): a method
/// descriptor never holds one, and a row key holding no NUL names no class
/// ids, so `MethodTypeTable::forget_classes` never drops a site row (it names
/// only built-in loaders' classes, which never unload).
pub const MH_SITE_TYPE_KEY_TAG: char = '\u{1}';

/// The table key of the call site `desc` resolved for a caller whose defining
/// loader is `loader` (`loader_id_of_class_definite`): only a KNOWN built-in
/// loader (0 bootstrap, 1 platform, 2 application), whose classes never unload
/// (a row is rooted outright, like every row of the table); `None` for any
/// other loader, an unknown one, or a text that is not a method descriptor.
pub fn mh_site_type_key(loader: Option<i32>, desc: &str) -> Option<String> {
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
pub fn mh_site_type_row_admissible(
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
pub fn mh_site_type_components_builtin(
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
pub fn mh_site_type_component_builtin(
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
pub fn mh_is_vm_carrier_without_kind(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    crate::panama::is_downcall_handle(ctx, mh)
}

/// `CRATONVM_MH_REAL_ADAPT_SKIPS_CARRIER` -- correctness, default ON, `=0`
/// lets [`mh_real_invoke_adapt`] run the JDK's `asType` on a VM-minted carrier
/// again (the `NoSuchMethodError: MethodHandle.editor()` of
/// `downcall.invoke(Integer.valueOf(-4))`). JIT round 17 wave 4, lane
/// mheditor. Read only for such a carrier at a converting call site.
pub fn mh_real_adapt_skips_carrier_enabled() -> bool {
    true
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
pub fn mh_with_arguments_runs_real(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    // The switch is read only for a downcall carrier (an uncached env read).
    mh_is_real_jdk_handle(ctx, this)
        || (mh_is_real_handle(ctx, this) && !mh_real_adapt_skips_carrier_enabled())
}

/// Is `this` the JDK's own variable-arity wrapper
/// (`MethodHandleImpl$AsVarargsCollector`), the one class that overrides
/// `invokeWithArguments(Object...)` (JDK 25)? Reads only. Interpreter round i4
/// wave 84, lane L4.
pub fn mh_is_real_varargs_collector(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
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
/// `--real-jdk` every `asType` of a downcall handle (the registered `asType`
/// `Bridge` yields to the real `asType` bytecode, which calls this), and in
/// both modes the `asType` inside the JDK's `invokeWithArguments`. The carrier
/// answers the adapted copy the `asType` `Bridge` mints
/// ([`mh_downcall_as_type`], HotSpot's refusals included). Every other
/// receiver runs the JDK's own body unchanged (`AsVarargsCollector`, which
/// overrides this method, never reaches here). `None`: not a carrier, or
/// `CRATONVM_MH_REAL_ADAPT_SKIPS_CARRIER=0`.
pub fn mh_as_type_uncached_carrier(
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
// `getter.bindTo(box).invoke()` under `--real-jdk` adapts every FRESH bound
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
pub fn mh_real_as_type_memo_enabled() -> bool {
    crate::nbflags().mh_real_as_type_memo
}

/// Lines of a thread's `asType` memo (a power of two, direct-mapped).
pub const AS_TYPE_MEMO_SLOTS: usize = 64;
/// `MethodHandle.asTypeUncached`'s descriptor.
pub const AS_TYPE_UNCACHED_DESC: &str =
    "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
/// The bound-handle species' class-name prefix (JDK 25: `Species_L` is
/// declared in `BoundMethodHandle`, the others are spun with this name).
pub const AS_TYPE_SPECIES_PREFIX: &str = "java/lang/invoke/BoundMethodHandle$Species_";

/// The admitted receiver shapes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AsTypeReceiver {
    /// A `DirectMethodHandle` of any species: `rebind()` is a reinvoker.
    Direct,
    /// A bound species: `rebind()` is itself (or a reinvoker when too complex).
    Bound,
}

/// The receiver shape of a class named `name`, or `None` (not admitted).
pub fn as_type_memo_receiver_kind(name: &str) -> Option<AsTypeReceiver> {
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
pub enum AsTypeSource {
    /// The receiver itself (a reinvoker's `argL0`).
    Receiver,
    /// The receiver's bound field of the same name.
    ReceiverField,
    /// The filed answer's own value (a shared conversion function).
    Constant,
}

/// One bound field of the filed species.
#[derive(Clone, Debug)]
pub struct AsTypeField {
    pub name: Box<str>,
    pub source: AsTypeSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AsTypeState {
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
pub fn as_type_memo_next_state(
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
pub struct AsTypeMemoKey {
    pub recv_class: u32,
    pub recv_type: usize,
    pub recv_form: usize,
    pub new_type: usize,
}

/// One filed answer, valid for VM `vm` in collection epoch `epoch` only.
#[derive(Clone)]
pub struct AsTypeMemoLine {
    pub vm: usize,
    pub epoch: u64,
    pub key: AsTypeMemoKey,
    pub state: AsTypeState,
    /// The candidate's receiver (a promotion needs a different one).
    pub first_receiver: usize,
    pub answer: ObjectRef,
    pub answer_class: ClassId,
    pub form: Value,
    pub fields: std::rc::Rc<[AsTypeField]>,
}

thread_local! {
    /// Per-thread `asType` adapter memo (see the section comment).
    pub static AS_TYPE_MEMO: std::cell::RefCell<Vec<Option<AsTypeMemoLine>>> =
        std::cell::RefCell::new(vec![None; AS_TYPE_MEMO_SLOTS]);
}

/// The memo line of `key` (a mixed hash, as [`finder_mint_slot`]).
pub fn as_type_memo_slot(key: &AsTypeMemoKey) -> usize {
    let mut h = u64::from(key.recv_class)
        ^ (key.recv_type as u64).rotate_left(17)
        ^ (key.recv_form as u64).rotate_left(31)
        ^ (key.new_type as u64).rotate_left(47);
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((h ^ (h >> 31)) as usize) & (AS_TYPE_MEMO_SLOTS - 1)
}

/// Two field values the same: references by identity, primitives by bits.
pub fn as_type_memo_same_value(a: Value, b: Value) -> bool {
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
pub fn as_type_memo_is(value: Value, o: ObjectRef) -> bool {
    matches!(value, Value::Object(Some(v)) if v.as_ptr() == o.as_ptr())
}

/// The instance fields `class` itself declares, or `None` when a class between
/// it and `java/lang/invoke/MethodHandle` declares any (a layout this memo does
/// not know). Reads only.
pub fn as_type_memo_own_fields(
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
pub fn as_type_memo_candidate(
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
pub fn as_type_memo_predicts(
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
pub fn as_type_memo_mint(
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
/// `None` when the memo does not apply (switch off, not `--real-jdk`, a null
/// type, a receiver class not admitted): the caller runs the body.
pub fn mh_real_as_type_memo(
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
pub fn mh_real_exact_refusal(
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
pub fn mh_exact_loader_check_enabled() -> bool {
    true
}

/// Does `desc` name a class, outside `java/` (which only the boot and
/// platform loaders define), for which `f` answers `true`? The names are the
/// reference components' (array elements unwrapped). No allocation.
pub fn mh_descriptor_any_loader_name(desc: &str, mut f: impl FnMut(&str) -> bool) -> bool {
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
pub fn mh_site_names_ambiguous_class(ctx: &dyn NativeContext, site: &str) -> bool {
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
pub fn mh_exact_loader_refusal(
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
pub fn mh_element_class_id(ctx: &dyn NativeContext, id: ClassId) -> ClassId {
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
pub fn mh_type_class_ids(ctx: &dyn NativeContext, mt: ObjectRef) -> Vec<ClassId> {
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
pub fn mh_wmte_message_icce(
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
pub fn mh_mirror_seen_differently(
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
pub fn mh_door_initiated_view_enabled() -> bool {
    true
}

/// `CRATONVM_MH_SITE_ACCESS_CHECK` -- correctness, default ON, `=0` restores
/// round 17 wave 6's first answer (JIT round 17 wave 6, lane mh6, second
/// pass): see [`mh_site_access_refusal`]. Read only for a site naming a class
/// two loaders define.
pub fn mh_site_access_check_enabled() -> bool {
    true
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
pub fn mh_site_access_refusal(ctx: &dyn NativeContext, site: Option<&str>) -> Option<MethodCallFailed> {
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
pub fn mh_caller_sees(ctx: &dyn NativeContext, name: &str, caller: ClassId) -> Option<ClassId> {
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
pub fn mh_invoke_loader_cast_enabled() -> bool {
    true
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
pub fn mh_invoke_loader_cast_refusal(
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
pub fn mh_invoke_loader_cast_full_enabled() -> bool {
    true
}

/// Is an object of class `from` an instance of the ARRAY class `to` (JVMS
/// §6.5 `checkcast`, arrays): `from` an array of as many dimensions or more,
/// each element class an instance of the other's, down to the leaves; an
/// array deeper than `to` fits an `Object` / `Cloneable` / `Serializable`
/// leaf. An unknown name or component answers `true` (the doors' accept). JIT
/// round 17 wave 3, lane mh4.
pub fn mh_array_instance_of(ctx: &dyn NativeContext, from: ClassId, to: ClassId) -> bool {
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
pub fn mh_invoke_return_cast_target(
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
pub fn mh_invoke_return_cast(
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
/// ran where HotSpot raises `WrongMethodTypeException` (`--real-jdk`).
pub fn mh_real_unspellable_exact_refusal(
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
pub fn primitive_char(token: &str) -> Option<u8> {
    let b = token.as_bytes();
    match (b.len(), b.first()) {
        (1, Some(c @ (b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D' | b'Z'))) => Some(*c),
        _ => None,
    }
}

/// JLS 5.1.2 widening primitive conversion: can `from` reach `to` without a
/// cast? `boolean` reaches nothing and nothing reaches it.
pub fn primitive_widens(from: u8, to: u8) -> bool {
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
pub fn reference_arg_admitted(
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
pub fn reference_array_target_admitted(
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
pub fn reference_cast_failure(
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
pub const LEAF_CASTS_REFERENCES: bool = true;

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
pub fn leaf_reference_refusal(
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
        return Some(reference_cast_failure(ctx, value, pdesc));    }
    None
}

/// [`leaf_reference_refusal`] for the RECEIVER of an unbound leaf handle
/// (`VIRTUAL`, `SPECIAL`, an instance `GETTER` / `SETTER`), whose type is the
/// handle's `MH_CLASS` (an internal class name, or an array descriptor for an
/// array class) -- `DirectMethodHandle.checkReceiver` / `checkBase` on
/// HotSpot. The exact-class case is answered before a descriptor is built.
pub fn leaf_receiver_refusal(
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
pub fn special_receiver_refusal_named(
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
pub fn invoke_reference_cast_refusal(
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
pub fn invoke_reference_cast_refusal_with(
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

