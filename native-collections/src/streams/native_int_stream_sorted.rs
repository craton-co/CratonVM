#[allow(unused_imports)]
use super::*;

/// `IntStream.sorted()` — natural ascending order. The synthetic IntStream
/// created by `range`/`rangeClosed`/`map`/etc. (this handler set) had no
/// `sorted`, so it dispatched to the abstract `IntStream.sorted()` → "has no
/// Code attribute" AME. Groovy's shaded ANTLR4 lexer (`LexerActionExecutor`)
/// calls it, so every Groovy script failed to compile
/// (SpringRepositoriesExtensionTests).
pub(crate) fn native_int_stream_sorted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let mut elements = int_stream_elements(ctx, &mut this);
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::SortedNatural);
    elements.sort_by_key(|v| match v {
        Value::Int(i) => *i,
        _ => 0,
    });
    let r = make_int_stream(ctx, &elements);
    with_derived_spl_facts(ctx, r, prim_facts)
}

/// `IntStream.distinct()` — the first occurrence of each value, in encounter
/// order.
///
/// W7-2 §7.2: `distinct` was missing on ALL THREE primitive streams, which on a
/// synthetic stream (whose runtime class is the interface itself) means the
/// `invokeinterface` resolved to the abstract declaration and the call died with
/// `AbstractMethodError: … has no Code attribute` — the same death
/// `summaryStatistics` died, and the family this record is about.
///
/// The JDK's `IntPipeline.distinct()` is `boxed().distinct().mapToInt(i -> i)`,
/// i.e. `Integer.equals`, which for `Integer` is plain value equality — so a set
/// of the raw `i32`s is exactly it. Encounter order is preserved by keeping the
/// FIRST occurrence, which is what `ReduceOps`' `LinkedHashSet` accumulator does.
/// A non-`Int` element cannot occur on a primitive stream; it is passed through
/// rather than silently dropped, so a layout defect elsewhere shows up as a
/// wrong count instead of as an empty stream.
pub(crate) fn native_int_stream_distinct(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let elements = int_stream_elements(ctx, &mut this);
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::PrimDistinct);
    let mut seen: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(elements.len());
    for e in &elements {
        match e {
            Value::Int(i) => {
                if seen.insert(*i) {
                    out.push(*e);
                }
            }
            _ => out.push(*e),
        }
    }
    let r = make_int_stream(ctx, &out);
    with_derived_spl_facts(ctx, r, prim_facts)
}

pub(crate) fn native_int_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let pred_pin = ctx.pin_native_root(predicate);
    let elements = int_stream_elements(ctx, &mut this);
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Filter);
    let mut kept = Vec::new();
    for elem in &elements {
        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let result = match ctx.invoke_virtual(predicate, "test", "(I)Z", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(pred_pin);
                return Err(e);
            }
        };
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            kept.push(*elem);
        }
    }
    ctx.unpin_native_roots(pred_pin);
    let r = make_int_stream(ctx, &kept);
    with_derived_spl_facts(ctx, r, prim_facts)
}

pub(crate) fn native_int_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(operator);
    let elements = int_stream_elements(ctx, &mut this);
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Map);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let operator = ctx.read_native_pin(op_pin, operator);
        let result = match ctx.invoke_virtual(operator, "applyAsInt", "(I)I", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        };
        mapped.push(result.unwrap_or(Value::Int(0)));    }
    ctx.unpin_native_roots(op_pin);
    let r = make_int_stream(ctx, &mapped);
    with_derived_spl_facts(ctx, r, prim_facts)
}

/// IntStream.mapToObj(IntFunction) → Stream<T>. The synthetic IntStream has
/// no real bytecode, so without this native the call resolves to the
/// abstract interface declaration — `AbstractMethodError: mapToObj has no
/// Code attribute`. JUnit5 jupiter-params hits this for every primitive
/// @ValueSource (`IntStream.range(0, n).mapToObj(i -> Array.get(src, i))`).
pub(crate) fn native_int_stream_map_to_obj(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let fn_pin = ctx.pin_native_root(function);
    let elements = int_stream_elements(ctx, &mut this);
    // Wave 9 (lane spliter): read before the mapper runs and moves `this`.
    let spl_facts = stream_derive_spl_facts(&*ctx, this, SplOp::Map);
    let mut mapped = Vec::with_capacity(elements.len());
    let mut mapped_handles: Vec<usize> = Vec::with_capacity(elements.len());
    for elem in &elements {
        let function = ctx.read_native_pin(fn_pin, function);
        let result = match ctx.invoke_virtual(function, "apply", "(I)Ljava/lang/Object;", &[*elem])
        {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(fn_pin);
                return Err(e);
            }
        };
        let v = result.unwrap_or(Value::Object(None));
        mapped_handles.push(pin_value(ctx, v));
        mapped.push(v);
    }
    let mapped = read_value_slice(ctx, &mapped_handles, &mapped);
    ctx.unpin_native_roots(fn_pin);
    let r = make_stream(ctx, &mapped);
    with_derived_spl_facts(ctx, r, spl_facts)
}

pub(crate) fn native_int_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = int_stream_elements(ctx, &mut this);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// Box each primitive stream element to its wrapper object so `boxed()` yields a
/// real `Stream<Integer/Long/Double>` — collect/iteration then observe wrapper
/// objects, not raw primitive `Value`s in reference slots.
///
/// Reference elements are returned verbatim and allocate nothing, so this is
/// safe to run over any element list whose destination is reference-typed
/// (`Object[]`, `Iterator.next()Ljava/lang/Object;`) without asking first
/// whether the source stream is primitive.
///
/// Fallible: the per-element boxing is [`box_primitive_result`], whose
/// `valueOf`-then-fabricate path can refuse (a fabricated `java/lang/Integer`
/// is refused under `--real-jdk`). Returning the refusal is the point — the
/// previous `.ok().flatten().unwrap_or(Value::Object(None))` turned it into a
/// silent `null` element, which is a wrong answer wearing the shape of a
/// correct one.
pub(crate) fn box_primitive_stream_elements(
    ctx: &mut dyn NativeContext,
    elements: &[Value],
) -> Result<Vec<Value>, MethodCallFailed> {
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — each
    // `valueOf` allocates, so the wrapper refs accumulated in `out` (and any
    // object elements passed through) must be pinned as produced and re-read
    // on return.
    let (elem_base, elem_handles) = pin_value_slice(ctx, elements);
    let mut first_pin = elem_base;
    let mut out = Vec::with_capacity(elements.len());
    let mut out_handles: Vec<usize> = Vec::with_capacity(elements.len());
    // Single-exit: an early `return Err` would leak every pin taken above, so
    // the failure is parked and raised after the unpin below.
    let mut failure: Option<MethodCallFailed> = None;
    for (idx, v) in elements.iter().enumerate() {
        let v = read_pinned_elem(ctx, elem_handles[idx], *v);
        let bv = match box_primitive_result(ctx, v) {
            Ok(bv) => bv,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        let h = pin_value(ctx, bv);
        if first_pin == usize::MAX {
            first_pin = h;
        }
        out_handles.push(h);
        out.push(bv);
    }
    let out = read_value_slice(ctx, &out_handles, &out);
    if first_pin != usize::MAX {
        ctx.unpin_native_roots(first_pin);
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

pub(crate) fn native_int_stream_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = int_stream_elements(ctx, &mut this);
    // Wave 9 (lane spliter): `boxed()` keeps every flag (`mapToObj(.., 0)`).
    // Read before boxing allocates and moves `this`.
    let spl_facts = stream_derive_spl_facts(&*ctx, this, SplOp::Preserve);
    let boxed = box_primitive_stream_elements(ctx, &elements)?;
    let r = make_stream(ctx, &boxed);
    with_derived_spl_facts(ctx, r, spl_facts)
}

pub(crate) fn native_int_stream_average(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = int_stream_elements(ctx, &mut this);
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    if !elements.is_empty() {
        let sum: i64 = elements
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i as i64,
                _ => 0,
            })
            .sum();
        let avg = sum as f64 / elements.len() as f64;
        set_opt_prim_value(ctx, opt, Value::Double(avg));
    }
    Ok(Some(Value::Object(Some(opt))))
}

// ===========================================================================
// LongStream — same 1-field layout, backing array stores Value::Long
// ===========================================================================

pub(in crate::streams) fn make_long_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let (elem_base, elem_handles) = pin_value_slice(ctx, elements);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/LongStream", STREAM_NUM_FIELDS)?;
    let stream_pin = ctx.pin_native_root(stream);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, elements.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elements.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    let stream = ctx.read_native_pin(stream_pin, stream);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        stream_pin
    } else {
        elem_base
    });
    Ok(Some(Value::Object(Some(stream))))
}

// LongStream — missing ops (element Value::Long via stream_elements). See the
// IntStream block above for rationale (synthetic stream → AbstractMethodError).
pub(crate) fn native_long_stream_map_to_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let op = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(op);
    let els = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let mut out = Vec::with_capacity(els.len());
    for e in &els {
        let op = ctx.read_native_pin(op_pin, op);
        match ctx.invoke_virtual(op, "applyAsInt", "(J)I", &[*e]) {
            Ok(r) => out.push(r.unwrap_or(Value::Int(0))),
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(op_pin);
    make_int_stream(ctx, &out)
}
pub(crate) fn native_long_stream_map_to_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let op = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(op);
    let els = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let mut out = Vec::with_capacity(els.len());
    for e in &els {
        let op = ctx.read_native_pin(op_pin, op);
        match ctx.invoke_virtual(op, "applyAsDouble", "(J)D", &[*e]) {
            Ok(r) => out.push(r.unwrap_or(Value::Double(0.0))),
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(op_pin);
    make_double_stream(ctx, &out)
}
pub(crate) fn native_long_stream_any_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let p = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    stream_match(ctx, &mut this, p, "(J)Z", 0)}
pub(crate) fn native_long_stream_all_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let p = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    stream_match(ctx, &mut this, p, "(J)Z", 1)}
pub(crate) fn native_long_stream_none_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let p = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    stream_match(ctx, &mut this, p, "(J)Z", 2)}
pub(crate) fn native_long_stream_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_count_refused(args)?;
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let n = stream_limit_n(args);
    let els = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Preserve);
    let r = make_long_stream(ctx, &els[..n.min(els.len())]);
    with_derived_spl_facts(ctx, r, prim_facts)
}
pub(crate) fn native_long_stream_skip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_count_refused(args)?;
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let n = stream_limit_n(args);
    let els = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Preserve);
    let r = make_long_stream(ctx, &els[n.min(els.len())..]);
    with_derived_spl_facts(ctx, r, prim_facts)
}
pub(crate) fn native_long_stream_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let cn = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let cn_pin = cn.map(|r| ctx.pin_native_root(r));
    let els = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            if let Some(p) = cn_pin {
                ctx.unpin_native_roots(p);            }
            return Err(e);
        }
    };
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Preserve);
    if let (Some(cn), Some(p)) = (cn, cn_pin) {
        for e in &els {
            let cn = ctx.read_native_pin(p, cn);
            if let Err(e) = ctx.invoke_virtual(cn, "accept", "(J)V", &[*e]) {
                ctx.unpin_native_roots(p);
                return Err(e);
            }
        }
        ctx.unpin_native_roots(p);
    }
    let r = make_long_stream(ctx, &els);
    with_derived_spl_facts(ctx, r, prim_facts)
}
pub(crate) fn native_long_stream_flat_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let f = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let f_pin = ctx.pin_native_root(f);
    // `f_pin` is live, so this unwinds exactly as the `apply` arm below does.
    let els = match prim_stream_values(ctx, &mut this, "()[J") {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(f_pin);
            return Err(e);
        }
    };
    let mut out = Vec::new();
    for e in &els {
        let f = ctx.read_native_pin(f_pin, f);
        match ctx.invoke_virtual(f, "apply", "(J)Ljava/lang/Object;", &[*e]) {
            Ok(Some(Value::Object(Some(mut sub)))) => {
                match prim_stream_values(ctx, &mut sub, "()[J") {
                    Ok(v) => out.extend(v),
                    Err(e) => {
                        ctx.unpin_native_roots(f_pin);
                        return Err(e);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                ctx.unpin_native_roots(f_pin);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(f_pin);
    make_long_stream(ctx, &out)
}
/// `LongStream.sorted()` — natural ascending order.
///
/// W7-2 §7.2. `sorted` existed for `IntStream` only (added for Groovy's shaded
/// ANTLR lexer); the `LongStream` and `DoubleStream` declarations were abstract
/// and unregistered, so the call died with `AbstractMethodError: … has no Code
/// attribute` on the interface exactly as `summaryStatistics` did.
pub(crate) fn native_long_stream_sorted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let mut elements = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::SortedNatural);
    elements.sort_by_key(|v| match v {
        Value::Long(l) => *l,
        Value::Int(i) => *i as i64,
        _ => 0,
    });
    let r = make_long_stream(ctx, &elements);
    with_derived_spl_facts(ctx, r, prim_facts)
}

/// `LongStream.distinct()` — see [`native_int_stream_distinct`]. `Long.equals`
/// is value equality, so a set of the raw `i64`s is the JDK's dedupe.
pub(crate) fn native_long_stream_distinct(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::PrimDistinct);
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(elements.len());
    for e in &elements {
        match e {
            Value::Long(l) => {
                if seen.insert(*l) {
                    out.push(*e);
                }
            }
            Value::Int(i) => {
                if seen.insert(*i as i64) {
                    out.push(Value::Long(*i as i64));                }            }
            _ => out.push(*e),
        }
    }
    let r = make_long_stream(ctx, &out);
    with_derived_spl_facts(ctx, r, prim_facts)
}

/// `LongStream.findFirst()` / `findAny()` — `OptionalLong` of the first element.
///
/// W7-2 §7.2: `IntStream` got this in spring-bug-03 and the other two were left,
/// which is the "each was fixed for the one member a workload happened to reach"
/// shape §1 of that record names. The synthetic stream is eager and
/// `stream_elements` preserves encounter order, so the first element satisfies
/// `findFirst`; `findAny` may return any element and the first is a valid choice.
pub(crate) fn native_long_stream_find_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalLong", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let first = match elements.first() {
        Some(Value::Long(l)) => Some(Value::Long(*l)),
        Some(Value::Int(i)) => Some(Value::Long(*i as i64)),
        _ => None,
    };
    let opt = make_opt_prim(ctx, "java/util/OptionalLong", None)?;
    if let Some(v) = first {
        set_opt_prim_value(ctx, opt, v);
    }
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn register_long_stream_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/stream/LongStream";
    r.register(
        c,
        "mapToInt",
        "(Ljava/util/function/LongToIntFunction;)Ljava/util/stream/IntStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_long_stream_map_to_int,
                StageClass::LongMapToInt,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "mapToDouble",
        "(Ljava/util/function/LongToDoubleFunction;)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_long_stream_map_to_double,
                StageClass::LongMapToDouble,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "anyMatch",
        "(Ljava/util/function/LongPredicate;)Z",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_any_match), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "allMatch",
        "(Ljava/util/function/LongPredicate;)Z",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_all_match), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "noneMatch",
        "(Ljava/util/function/LongPredicate;)Z",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_none_match), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "limit",
        "(J)Ljava/util/stream/LongStream;",
        |ctx, args| {
            stream_slice_op(
                ctx,
                args,
                native_long_stream_limit,
                StageClass::LongSlice,
                false,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "skip",
        "(J)Ljava/util/stream/LongStream;",
        |ctx, args| {
            stream_slice_op(
                ctx,
                args,
                native_long_stream_skip,
                StageClass::LongSlice,
                true,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "peek",
        "(Ljava/util/function/LongConsumer;)Ljava/util/stream/LongStream;",
        |ctx, args| stream_stage_op(ctx, args, native_long_stream_peek, StageClass::LongPeek), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "flatMap",
        "(Ljava/util/function/LongFunction;)Ljava/util/stream/LongStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_long_stream_flat_map,
                StageClass::LongFlatMap,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        c,
        "of",
        "(J)Ljava/util/stream/LongStream;",
        native_long_stream_of, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "range",
        "(JJ)Ljava/util/stream/LongStream;",
        native_long_stream_range, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "rangeClosed",
        "(JJ)Ljava/util/stream/LongStream;",
        native_long_stream_range_closed, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "sum", "()J", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_sum)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "reduce",
        "(JLjava/util/function/LongBinaryOperator;)J",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_reduce_seeded), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "reduce",
        "(Ljava/util/function/LongBinaryOperator;)Ljava/util/OptionalLong;",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_reduce), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "count", "()J", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_count)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "collect",
        "(Ljava/util/function/Supplier;Ljava/util/function/ObjLongConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_collect), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "min", "()Ljava/util/OptionalLong;", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_min)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "max", "()Ljava/util/OptionalLong;", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_max)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "average", "()Ljava/util/OptionalDouble;", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_average)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/LongConsumer;)V",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_for_each), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "filter",
        "(Ljava/util/function/LongPredicate;)Ljava/util/stream/LongStream;",
        |ctx, args| stream_stage_op(ctx, args, native_long_stream_filter, StageClass::LongFilter), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "map",
        "(Ljava/util/function/LongUnaryOperator;)Ljava/util/stream/LongStream;",
        |ctx, args| stream_stage_op(ctx, args, native_long_stream_map, StageClass::LongMap), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "toArray", "()[J", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_to_array)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Kind stated, not inherited (JIT round 17 wave 1, lane streams,
    // proposal COL8-1): see `primitive_stream_iterator_kind`.
    r.register(
        c,
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfLong;",
        |ctx, args| stream_terminal_op(ctx, args, native_long_stream_iterator),
        primitive_stream_iterator_kind(),
    );
    r.register(c, "boxed", "()Ljava/util/stream/Stream;", |ctx, args| {
        stream_stage_op(
            ctx,
            args,
            native_long_stream_boxed,
            StageClass::LongMapToObj,
        )
    }, cratonvm_native_api::NativeKind::Bridge);
    // mapToObj(LongFunction) -> Stream. Without this real-JDK-active native, an
    // `invokeinterface LongStream.mapToObj` on our synthetic LongStream resolved
    // to the bodiless interface method (`AbstractMethodError: … has no Code
    // attribute`) — the synthetic-jdk-only `register_phase56_stream_extras`
    // registration is compiled out of the real-JDK CLI.
    r.register(
        c,
        "mapToObj",
        "(Ljava/util/function/LongFunction;)Ljava/util/stream/Stream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_long_stream_map_to_obj,
                StageClass::LongMapToObj,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "asDoubleStream",
        "()Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_long_stream_as_double,
                StageClass::LongAsDouble,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // W7-2 §7.2 — the LongStream half of the hole: `findFirst`/`findAny`
    // (present on IntStream since spring-bug-03), `sorted` and `distinct`.
    r.register(c, "findFirst", "()Ljava/util/OptionalLong;", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_find_first)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "findAny", "()Ljava/util/OptionalLong;", |ctx, args| {
        stream_terminal_op(ctx, args, native_long_stream_find_first)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "sorted",
        "()Ljava/util/stream/LongStream;",
        |ctx, args| stream_stage_op(ctx, args, native_long_stream_sorted, StageClass::LongSorted), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "distinct",
        "()Ljava/util/stream/LongStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_long_stream_distinct,
                StageClass::RefMapToLong,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

pub(crate) fn native_long_stream_map_to_obj(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return make_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let fn_pin = ctx.pin_native_root(mapper);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(fn_pin);
            return Err(e);
        }
    };
    let mut mapped = Vec::with_capacity(elements.len());
    let mut mapped_handles: Vec<usize> = Vec::with_capacity(elements.len());
    for e in &elements {
        let mapper = ctx.read_native_pin(fn_pin, mapper);
        let v = ctx
            .invoke_virtual(mapper, "apply", "(J)Ljava/lang/Object;", &[*e])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None));
        mapped_handles.push(pin_value(ctx, v));
        mapped.push(v);
    }
    let mapped = read_value_slice(ctx, &mapped_handles, &mapped);
    ctx.unpin_native_roots(fn_pin);
    make_stream(ctx, &mapped)
}

pub(crate) fn native_long_stream_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = args.first().copied().unwrap_or(Value::Long(0));
    let r = make_long_stream(ctx, &[v]);
    with_one_element_source_spl_facts(ctx, r)
}

pub(crate) fn native_long_stream_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let start = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let end = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // Widen to i128 so `end - start` cannot overflow when start == i64::MIN.
    let count = (end as i128 - start as i128).max(0);
    let elems = range_long_elements(start, count)?;
    let r = make_long_stream(ctx, &elems);
    // Wave 9 (lane spliter): `Streams.RangeLongSpliterator`, or
    // `LongStream.empty()` for an empty range.
    let chars = if elems.is_empty() {
        SPL_EMPTY
    } else {
        SPL_RANGE
    };
    with_source_spl_facts(ctx, r, chars)
}

pub(crate) fn native_long_stream_range_closed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let start = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let end = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // Widen to i128 so `end - start + 1` cannot overflow at the i64 boundaries.
    let count = (end as i128 - start as i128 + 1).max(0);
    let elems = range_long_elements(start, count)?;
    let r = make_long_stream(ctx, &elems);
    // Wave 9 (lane spliter): `Streams.RangeLongSpliterator`, or
    // `LongStream.empty()` for an empty range.
    let chars = if elems.is_empty() {
        SPL_EMPTY
    } else {
        SPL_RANGE
    };
    with_source_spl_facts(ctx, r, chars)
}

pub(crate) fn native_long_stream_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = stream_elements(ctx, &mut this)?;
    // JDK `LongStream.sum()` has wrapping (two's-complement) overflow
    // semantics; fold with `wrapping_add` to match and avoid debug-build
    // panics from `Iterator::sum`.
    let sum: i64 = elements.iter().fold(0i64, |acc, v| match v {
        Value::Long(l) => acc.wrapping_add(*l),
        _ => acc,
    });
    Ok(Some(Value::Long(sum)))
}

pub(crate) fn native_long_stream_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = stream_elements(ctx, &mut this)?;
    Ok(Some(Value::Long(elements.len() as i64)))
}

pub(crate) fn native_long_stream_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalLong", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let opt = make_opt_prim(ctx, "java/util/OptionalLong", None)?;
    if let Some(min) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Long(l) => Some(*l),
            _ => None,
        })
        .min()
    {
        set_opt_prim_value(ctx, opt, Value::Long(min));
    }
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_long_stream_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalLong", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let opt = make_opt_prim(ctx, "java/util/OptionalLong", None)?;
    if let Some(max) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Long(l) => Some(*l),
            _ => None,
        })
        .max()
    {
        set_opt_prim_value(ctx, opt, Value::Long(max));
    }
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_long_stream_average(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    if !elements.is_empty() {
        let sum: i64 = elements
            .iter()
            .map(|v| match v {
                Value::Long(l) => *l,
                _ => 0,
            })
            .sum();
        let avg = sum as f64 / elements.len() as f64;
        set_opt_prim_value(ctx, opt, Value::Double(avg));
    }
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_long_stream_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let con_pin = ctx.pin_native_root(consumer);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(con_pin);
            return Err(e);
        }
    };
    for elem in &elements {
        let consumer = ctx.read_native_pin(con_pin, consumer);
        if let Err(e) = ctx.invoke_virtual(consumer, "accept", "(J)V", &[*elem]) {
            ctx.unpin_native_roots(con_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(con_pin);
    Ok(None)
}

/// `LongStream.collect(Supplier, ObjLongConsumer, BiConsumer)` — see
/// `native_int_stream_collect` for the rationale (missing 3-arg collect →
/// AbstractMethodError). `accept` takes `(Object, long)`.
pub(crate) fn native_long_stream_collect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let supplier = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let accumulator = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let sup_pin = ctx.pin_native_root(supplier);
    let acc_pin = ctx.pin_native_root(accumulator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(sup_pin);
            return Err(e);
        }
    };
    let supplier = ctx.read_native_pin(sup_pin, supplier);
    let container = match ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(c)))) => c,
        Ok(_) => {
            ctx.unpin_native_roots(sup_pin);
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            ctx.unpin_native_roots(sup_pin);
            return Err(e);
        }
    };
    let pin = ctx.pin_native_root(container);
    for elem in &elements {
        let accumulator = ctx.read_native_pin(acc_pin, accumulator);
        let c = ctx.read_native_pin(pin, container);
        if let Err(e) = ctx.invoke_virtual(
            accumulator,
            "accept",
            "(Ljava/lang/Object;J)V",
            &[Value::Object(Some(c)), *elem],
        ) {
            ctx.unpin_native_roots(sup_pin);
            return Err(e);
        }
    }
    let c = ctx.read_native_pin(pin, container);
    ctx.unpin_native_roots(sup_pin);
    Ok(Some(Value::Object(Some(c))))
}

pub(crate) fn native_long_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let pred_pin = ctx.pin_native_root(predicate);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(pred_pin);
            return Err(e);
        }
    };
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Filter);
    let mut kept = Vec::new();
    for elem in &elements {
        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let result = match ctx.invoke_virtual(predicate, "test", "(J)Z", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(pred_pin);
                return Err(e);
            }
        };
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            kept.push(*elem);
        }
    }
    ctx.unpin_native_roots(pred_pin);
    let r = make_long_stream(ctx, &kept);
    with_derived_spl_facts(ctx, r, prim_facts)
}

pub(crate) fn native_long_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(operator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Map);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let operator = ctx.read_native_pin(op_pin, operator);
        let result = match ctx.invoke_virtual(operator, "applyAsLong", "(J)J", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        };
        mapped.push(result.unwrap_or(Value::Long(0)));    }
    ctx.unpin_native_roots(op_pin);
    let r = make_long_stream(ctx, &mapped);
    with_derived_spl_facts(ctx, r, prim_facts)
}

pub(crate) fn native_long_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_long_stream_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    // Wave 9 (lane spliter): `boxed()` keeps every flag (`mapToObj(.., 0)`).
    // Read before boxing allocates and moves `this`.
    let spl_facts = stream_derive_spl_facts(&*ctx, this, SplOp::Preserve);
    let boxed = box_primitive_stream_elements(ctx, &elements)?;
    let r = make_stream(ctx, &boxed);
    with_derived_spl_facts(ctx, r, spl_facts)
}

pub(crate) fn native_long_stream_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_primitive_iterator(ctx, "java/util/PrimitiveIterator$OfLong", &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    make_primitive_iterator(ctx, "java/util/PrimitiveIterator$OfLong", &elements)
}

pub(crate) fn native_long_stream_as_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    let doubles: Vec<Value> = elements
        .iter()
        .map(|v| match v {
            Value::Long(l) => Value::Double(*l as f64),
            _ => Value::Double(0.0),
        })
        .collect();
    make_double_stream(ctx, &doubles)
}

// ===========================================================================
// DoubleStream — same 1-field layout, backing array stores Value::Double
// ===========================================================================

pub(in crate::streams) fn make_double_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let (elem_base, elem_handles) = pin_value_slice(ctx, elements);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/DoubleStream", STREAM_NUM_FIELDS)?;
    let stream_pin = ctx.pin_native_root(stream);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Double, elements.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elements.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    let stream = ctx.read_native_pin(stream_pin, stream);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        stream_pin
    } else {
        elem_base
    });
    Ok(Some(Value::Object(Some(stream))))
}

// DoubleStream — missing ops (element Value::Double). See IntStream block above.
pub(crate) fn native_double_stream_all_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let p = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    stream_match(ctx, &mut this, p, "(D)Z", 1)}
pub(crate) fn native_double_stream_none_match(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let p = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    stream_match(ctx, &mut this, p, "(D)Z", 2)}
pub(crate) fn native_double_stream_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_count_refused(args)?;
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let n = stream_limit_n(args);
    let els = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Preserve);
    let r = make_double_stream(ctx, &els[..n.min(els.len())]);
    with_derived_spl_facts(ctx, r, prim_facts)
}
pub(crate) fn native_double_stream_skip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_count_refused(args)?;
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let n = stream_limit_n(args);
    let els = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Preserve);
    let r = make_double_stream(ctx, &els[n.min(els.len())..]);
    with_derived_spl_facts(ctx, r, prim_facts)
}
pub(crate) fn native_double_stream_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let cn = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let cn_pin = cn.map(|r| ctx.pin_native_root(r));
    let els = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            if let Some(p) = cn_pin {
                ctx.unpin_native_roots(p);            }
            return Err(e);
        }
    };
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Preserve);
    if let (Some(cn), Some(p)) = (cn, cn_pin) {
        for e in &els {
            let cn = ctx.read_native_pin(p, cn);
            if let Err(e) = ctx.invoke_virtual(cn, "accept", "(D)V", &[*e]) {
                ctx.unpin_native_roots(p);
                return Err(e);
            }
        }
        ctx.unpin_native_roots(p);
    }
    let r = make_double_stream(ctx, &els);
    with_derived_spl_facts(ctx, r, prim_facts)
}
pub(crate) fn native_double_stream_flat_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let f = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let f_pin = ctx.pin_native_root(f);
    // `f_pin` is live, so this unwinds exactly as the `apply` arm below does.
    let els = match prim_stream_values(ctx, &mut this, "()[D") {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(f_pin);
            return Err(e);
        }
    };
    let mut out = Vec::new();
    for e in &els {
        let f = ctx.read_native_pin(f_pin, f);
        match ctx.invoke_virtual(f, "apply", "(D)Ljava/lang/Object;", &[*e]) {
            Ok(Some(Value::Object(Some(mut sub)))) => {
                match prim_stream_values(ctx, &mut sub, "()[D") {
                    Ok(v) => out.extend(v),
                    Err(e) => {
                        ctx.unpin_native_roots(f_pin);
                        return Err(e);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                ctx.unpin_native_roots(f_pin);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(f_pin);
    make_double_stream(ctx, &out)
}

/// `Double.doubleToLongBits(d)` — the canonicalising form, which is what
/// `Double.equals` and `Double.compare` are both specified in terms of.
///
/// Rust's `f64::to_bits` is `doubleToRawLongBits`: it preserves the payload of a
/// signalling or negatively-signed NaN, so two NaNs that Java calls equal would
/// key differently. Collapsing every NaN onto the canonical quiet pattern is the
/// difference between `DoubleStream.of(0.0/0.0, 0.0/0.0).distinct().count()`
/// answering `1` (HotSpot) and `2`.
pub(crate) fn java_double_to_long_bits(d: f64) -> i64 {
    cratonvm_types::jfp::double_to_long_bits(d) as i64
}

/// `Double.compare(d1, d2)` as an `Ordering`, and a total order — which is what
/// `Arrays.sort(double[])` (and therefore `DoubleStream.sorted()`) uses.
///
/// It is NOT `f64`'s `PartialOrd`, and the two disagree on the two cases a
/// `sorted()` test looks at first: Java orders `-0.0` strictly BELOW `+0.0`, and
/// puts every NaN at the top. `f64::total_cmp` is a third order again (it sorts
/// negatively-signed NaN below `-inf`), so neither stock comparator will do.
pub(crate) fn java_double_compare(a: f64, b: f64) -> std::cmp::Ordering {
    cratonvm_types::jfp::double_ordering(a, b)
}

/// Read a stream element as an `f64`, widening the integral shapes the way the
/// primitive-stream natives around here already do.
pub(crate) fn stream_elem_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Double(d) => Some(*d),
        Value::Float(f) => Some(*f as f64),
        Value::Int(i) => Some(*i as f64),
        Value::Long(l) => Some(*l as f64),
        _ => None,
    }
}

/// `DoubleStream.anyMatch(DoublePredicate)`.
///
/// W7-2 §7.2 calls this one conspicuous, and it is: `allMatch` and `noneMatch`
/// sit beside it in `register_double_stream_natives` and `anyMatch` did not,
/// so `DoubleStream.anyMatch` alone died with `AbstractMethodError: … has no
/// Code attribute` on the interface declaration.
pub(crate) fn native_double_stream_any_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let p = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    stream_match(ctx, &mut this, p, "(D)Z", 0)}

/// `double DoubleStream.reduce(double identity, DoubleBinaryOperator op)`.
/// Mirrors `native_long_stream_reduce_seeded`; see it for the pin contract.
pub(crate) fn native_double_stream_reduce_seeded(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let identity = args.get(1).and_then(stream_elem_as_f64).unwrap_or(0.0);
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(identity))),
    };
    let op = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(identity))),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(op);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let mut acc = identity;
    for elem in &elements {
        let e = match stream_elem_as_f64(elem) {
            Some(d) => d,
            None => continue,
        };
        let op = ctx.read_native_pin(op_pin, op);
        let r = match ctx.invoke_virtual(
            op,
            "applyAsDouble",
            "(DD)D",
            &[Value::Double(acc), Value::Double(e)],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        };
        acc = r.as_ref().and_then(stream_elem_as_f64).unwrap_or(acc);    }
    ctx.unpin_native_roots(op_pin);
    Ok(Some(Value::Double(acc)))
}

/// `OptionalDouble DoubleStream.reduce(DoubleBinaryOperator op)` — the
/// no-identity overload. Empty stream → empty `OptionalDouble`.
pub(crate) fn native_double_stream_reduce(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let o = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(o))));
        }
    };
    let op = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let o = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(o))));
        }
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(op);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let mut acc: Option<f64> = None;
    for elem in &elements {
        let e = match stream_elem_as_f64(elem) {
            Some(d) => d,
            None => continue,
        };
        acc = Some(match acc {
            None => e,
            Some(a) => {
                let op = ctx.read_native_pin(op_pin, op);
                let r = match ctx.invoke_virtual(
                    op,
                    "applyAsDouble",
                    "(DD)D",
                    &[Value::Double(a), Value::Double(e)],
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        ctx.unpin_native_roots(op_pin);
                        return Err(e);
                    }
                };
                r.as_ref().and_then(stream_elem_as_f64).unwrap_or(a)            }        });    }
    ctx.unpin_native_roots(op_pin);
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    if let Some(a) = acc {
        set_opt_prim_value(ctx, opt, Value::Double(a));
    }
    Ok(Some(Value::Object(Some(opt))))
}

/// `DoubleStream.findFirst()` / `findAny()` — see
/// [`native_long_stream_find_first`].
pub(crate) fn native_double_stream_find_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let first = elements.first().and_then(stream_elem_as_f64);
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    if let Some(d) = first {
        set_opt_prim_value(ctx, opt, Value::Double(d));
    }
    Ok(Some(Value::Object(Some(opt))))
}

/// `DoubleStream.sorted()` — `Arrays.sort(double[])` order, i.e.
/// [`java_double_compare`], not `f64`'s `PartialOrd`.
pub(crate) fn native_double_stream_sorted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::SortedNatural);
    // Normalised to `Value::Double` on the way out, not just sorted: the result
    // goes to `make_double_stream`, which allocates a `double[]`, so an integral
    // `Value` that slipped in from an upstream widening op would be stored into
    // a slot of the wrong width.
    let mut doubles: Vec<f64> = Vec::with_capacity(elements.len());
    for e in &elements {
        doubles.push(stream_elem_as_f64(e).unwrap_or(0.0));
    }
    doubles.sort_by(|a, b| java_double_compare(*a, *b));
    let out: Vec<Value> = doubles.into_iter().map(Value::Double).collect();
    let r = make_double_stream(ctx, &out);
    with_derived_spl_facts(ctx, r, prim_facts)
}

/// `DoubleStream.distinct()` — `Double.equals` semantics, see
/// [`java_double_to_long_bits`]: all NaNs are one value and `-0.0` is distinct
/// from `+0.0`, which is the opposite of what `==` on `f64` says for both.
pub(crate) fn native_double_stream_distinct(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::PrimDistinct);
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(elements.len());
    for e in &elements {
        match stream_elem_as_f64(e) {
            Some(d) => {
                if seen.insert(java_double_to_long_bits(d)) {
                    out.push(Value::Double(d));
                }
            }
            None => out.push(*e),
        }
    }
    let r = make_double_stream(ctx, &out);
    with_derived_spl_facts(ctx, r, prim_facts)
}

pub(crate) fn register_double_stream_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/stream/DoubleStream";
    r.register(
        c,
        "allMatch",
        "(Ljava/util/function/DoublePredicate;)Z",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_all_match), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "noneMatch",
        "(Ljava/util/function/DoublePredicate;)Z",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_none_match), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "limit",
        "(J)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_slice_op(
                ctx,
                args,
                native_double_stream_limit,
                StageClass::DoubleSlice,
                false,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "skip",
        "(J)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_slice_op(
                ctx,
                args,
                native_double_stream_skip,
                StageClass::DoubleSlice,
                true,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "peek",
        "(Ljava/util/function/DoubleConsumer;)Ljava/util/stream/DoubleStream;",
        |ctx, args| stream_stage_op(ctx, args, native_double_stream_peek, StageClass::DoublePeek), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "flatMap",
        "(Ljava/util/function/DoubleFunction;)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_double_stream_flat_map,
                StageClass::DoubleFlatMap,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        c,
        "of",
        "(D)Ljava/util/stream/DoubleStream;",
        native_double_stream_of, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "sum", "()D", |ctx, args| {
        stream_terminal_op(ctx, args, native_double_stream_sum)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "count", "()J", |ctx, args| {
        stream_terminal_op(ctx, args, native_double_stream_count)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "collect",
        "(Ljava/util/function/Supplier;Ljava/util/function/ObjDoubleConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_collect), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "min", "()Ljava/util/OptionalDouble;", |ctx, args| {
        stream_terminal_op(ctx, args, native_double_stream_min)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "max", "()Ljava/util/OptionalDouble;", |ctx, args| {
        stream_terminal_op(ctx, args, native_double_stream_max)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "average", "()Ljava/util/OptionalDouble;", |ctx, args| {
        stream_terminal_op(ctx, args, native_double_stream_average)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/DoubleConsumer;)V",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_for_each), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "filter",
        "(Ljava/util/function/DoublePredicate;)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_double_stream_filter,
                StageClass::DoubleFilter,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "map",
        "(Ljava/util/function/DoubleUnaryOperator;)Ljava/util/stream/DoubleStream;",
        |ctx, args| stream_stage_op(ctx, args, native_double_stream_map, StageClass::DoubleMap), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "toArray", "()[D", |ctx, args| {
        stream_terminal_op(ctx, args, native_double_stream_to_array)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Kind stated, not inherited (JIT round 17 wave 1, lane streams,
    // proposal COL8-1): see `primitive_stream_iterator_kind`.
    r.register(
        c,
        "iterator",
        "()Ljava/util/PrimitiveIterator$OfDouble;",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_iterator),
        primitive_stream_iterator_kind(),
    );
    r.register(c, "boxed", "()Ljava/util/stream/Stream;", |ctx, args| {
        stream_stage_op(
            ctx,
            args,
            native_double_stream_boxed,
            StageClass::DoubleMapToObj,
        )
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "mapToObj",
        "(Ljava/util/function/DoubleFunction;)Ljava/util/stream/Stream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_double_stream_map_to_obj,
                StageClass::DoubleMapToObj,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "mapToLong",
        "(Ljava/util/function/DoubleToLongFunction;)Ljava/util/stream/LongStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_double_stream_map_to_long,
                StageClass::DoubleMapToLong,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "mapToInt",
        "(Ljava/util/function/DoubleToIntFunction;)Ljava/util/stream/IntStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_double_stream_map_to_int,
                StageClass::DoubleMapToInt,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // W7-2 §7.2 — the DoubleStream half, and the largest of the three holes:
    // `anyMatch` (conspicuous, because `allMatch`/`noneMatch` above it are both
    // registered), both `reduce` overloads, `findFirst`/`findAny`, `sorted` and
    // `distinct`. Every one of them was an abstract interface declaration with
    // no live registration, i.e. an `AbstractMethodError` waiting for a caller.
    r.register(
        c,
        "anyMatch",
        "(Ljava/util/function/DoublePredicate;)Z",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_any_match), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "reduce",
        "(DLjava/util/function/DoubleBinaryOperator;)D",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_reduce_seeded), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "reduce",
        "(Ljava/util/function/DoubleBinaryOperator;)Ljava/util/OptionalDouble;",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_reduce), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "findFirst",
        "()Ljava/util/OptionalDouble;",
        |ctx, args| stream_terminal_op(ctx, args, native_double_stream_find_first), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "findAny", "()Ljava/util/OptionalDouble;", |ctx, args| {
        stream_terminal_op(ctx, args, native_double_stream_find_first)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "sorted",
        "()Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_double_stream_sorted,
                StageClass::DoubleSorted,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "distinct",
        "()Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            stream_stage_op(
                ctx,
                args,
                native_double_stream_distinct,
                StageClass::RefMapToDouble,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

pub(crate) fn native_double_stream_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = args.first().copied().unwrap_or(Value::Double(0.0));
    let r = make_double_stream(ctx, &[v]);
    with_one_element_source_spl_facts(ctx, r)
}

pub(crate) fn native_double_stream_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let elements = stream_elements(ctx, &mut this)?;
    let sum: f64 = elements
        .iter()
        .map(|v| match v {
            Value::Double(d) => *d,
            _ => 0.0,
        })
        .sum();
    Ok(Some(Value::Double(sum)))
}

pub(crate) fn native_double_stream_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = stream_elements(ctx, &mut this)?;
    Ok(Some(Value::Long(elements.len() as i64)))
}

/// `DoubleStream.min()`. Folds with [`java_double_min`], not `f64::min` —
/// Rust's is IEEE `minNum` and returns the *non*-NaN operand where Java's
/// `Math.min` (which this specification is defined in terms of) propagates
/// it. Measured against HotSpot 25
/// (`docs/feature-designs/real-jdk-mode.md`
/// §9.1 and §5): `DoubleStream.of(1.0, NaN, 3.0).min()` is
/// `OptionalDouble[NaN]` on HotSpot and was `OptionalDouble[1.0]` here.
pub(crate) fn native_double_stream_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    if let Some(min) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Double(d) => Some(*d),
            _ => None,
        })
        .reduce(java_double_min)
    {
        set_opt_prim_value(ctx, opt, Value::Double(min));
    }
    Ok(Some(Value::Object(Some(opt))))
}

/// `DoubleStream.max()` — the mirror of [`native_double_stream_min`], same
/// NaN-propagation rule via [`java_double_max`].
pub(crate) fn native_double_stream_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    if let Some(max) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Double(d) => Some(*d),
            _ => None,
        })
        .reduce(java_double_max)
    {
        set_opt_prim_value(ctx, opt, Value::Double(max));
    }
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_double_stream_average(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    if !elements.is_empty() {
        let sum: f64 = elements
            .iter()
            .map(|v| match v {
                Value::Double(d) => *d,
                _ => 0.0,
            })
            .sum();
        let avg = sum / elements.len() as f64;
        set_opt_prim_value(ctx, opt, Value::Double(avg));
    }
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_double_stream_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let con_pin = ctx.pin_native_root(consumer);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(con_pin);
            return Err(e);
        }
    };
    for elem in &elements {
        let consumer = ctx.read_native_pin(con_pin, consumer);
        if let Err(e) = ctx.invoke_virtual(consumer, "accept", "(D)V", &[*elem]) {
            ctx.unpin_native_roots(con_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(con_pin);
    Ok(None)
}

/// `DoubleStream.collect(Supplier, ObjDoubleConsumer, BiConsumer)` — see
/// `native_int_stream_collect` for rationale. `accept` takes `(Object, double)`.
pub(crate) fn native_double_stream_collect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let supplier = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let accumulator = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let sup_pin = ctx.pin_native_root(supplier);
    let acc_pin = ctx.pin_native_root(accumulator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(sup_pin);
            return Err(e);
        }
    };
    let supplier = ctx.read_native_pin(sup_pin, supplier);
    let container = match ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(c)))) => c,
        Ok(_) => {
            ctx.unpin_native_roots(sup_pin);
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            ctx.unpin_native_roots(sup_pin);
            return Err(e);
        }
    };
    let pin = ctx.pin_native_root(container);
    for elem in &elements {
        let accumulator = ctx.read_native_pin(acc_pin, accumulator);
        let c = ctx.read_native_pin(pin, container);
        if let Err(e) = ctx.invoke_virtual(
            accumulator,
            "accept",
            "(Ljava/lang/Object;D)V",
            &[Value::Object(Some(c)), *elem],
        ) {
            ctx.unpin_native_roots(sup_pin);
            return Err(e);
        }
    }
    let c = ctx.read_native_pin(pin, container);
    ctx.unpin_native_roots(sup_pin);
    Ok(Some(Value::Object(Some(c))))
}

pub(crate) fn native_double_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let pred_pin = ctx.pin_native_root(predicate);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(pred_pin);
            return Err(e);
        }
    };
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Filter);
    let mut kept = Vec::new();
    for elem in &elements {
        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let result = match ctx.invoke_virtual(predicate, "test", "(D)Z", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(pred_pin);
                return Err(e);
            }
        };
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            kept.push(*elem);
        }
    }
    ctx.unpin_native_roots(pred_pin);
    let r = make_double_stream(ctx, &kept);
    with_derived_spl_facts(ctx, r, prim_facts)
}

pub(crate) fn native_double_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let op_pin = ctx.pin_native_root(operator);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(op_pin);
            return Err(e);
        }
    };
    let prim_facts = prim_stage_spl_facts(&*ctx, this, SplOp::Map);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let operator = ctx.read_native_pin(op_pin, operator);
        let result = match ctx.invoke_virtual(operator, "applyAsDouble", "(D)D", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(op_pin);
                return Err(e);
            }
        };
        mapped.push(result.unwrap_or(Value::Double(0.0)));    }
    ctx.unpin_native_roots(op_pin);
    let r = make_double_stream(ctx, &mapped);
    with_derived_spl_facts(ctx, r, prim_facts)
}

pub(crate) fn native_double_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Double, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = stream_elements(ctx, &mut this)?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Double, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_double_stream_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    // Wave 9 (lane spliter): `boxed()` keeps every flag (`mapToObj(.., 0)`).
    // Read before boxing allocates and moves `this`.
    let spl_facts = stream_derive_spl_facts(&*ctx, this, SplOp::Preserve);
    let boxed = box_primitive_stream_elements(ctx, &elements)?;
    let r = make_stream(ctx, &boxed);
    with_derived_spl_facts(ctx, r, spl_facts)
}

pub(crate) fn native_double_stream_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_primitive_iterator(ctx, "java/util/PrimitiveIterator$OfDouble", &[]),
    };
    let elements = stream_elements(ctx, &mut this)?;
    make_primitive_iterator(ctx, "java/util/PrimitiveIterator$OfDouble", &elements)
}

pub(crate) fn native_double_stream_map_to_obj(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return make_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let fn_pin = ctx.pin_native_root(mapper);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(fn_pin);
            return Err(e);
        }
    };
    let mut mapped = Vec::with_capacity(elements.len());
    let mut mapped_handles: Vec<usize> = Vec::with_capacity(elements.len());
    for e in &elements {
        let mapper = ctx.read_native_pin(fn_pin, mapper);
        let v = ctx
            .invoke_virtual(mapper, "apply", "(D)Ljava/lang/Object;", &[*e])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None));
        mapped_handles.push(pin_value(ctx, v));
        mapped.push(v);
    }
    let mapped = read_value_slice(ctx, &mapped_handles, &mapped);
    ctx.unpin_native_roots(fn_pin);
    make_stream(ctx, &mapped)
}

pub(crate) fn native_double_stream_map_to_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let fn_pin = ctx.pin_native_root(func);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(fn_pin);
            return Err(e);
        }
    };
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let func = ctx.read_native_pin(fn_pin, func);
        let result = match ctx.invoke_virtual(func, "applyAsLong", "(D)J", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(fn_pin);
                return Err(e);
            }
        };
        mapped.push(result.unwrap_or(Value::Long(0)));    }
    ctx.unpin_native_roots(fn_pin);
    make_long_stream(ctx, &mapped)
}

pub(crate) fn native_double_stream_map_to_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let fn_pin = ctx.pin_native_root(func);
    let elements = match stream_elements(ctx, &mut this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(fn_pin);
            return Err(e);
        }
    };
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let func = ctx.read_native_pin(fn_pin, func);
        let result = match ctx.invoke_virtual(func, "applyAsInt", "(D)I", &[*elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(fn_pin);
                return Err(e);
            }
        };
        mapped.push(result.unwrap_or(Value::Int(0)));    }
    ctx.unpin_native_roots(fn_pin);
    make_int_stream(ctx, &mapped)
}

