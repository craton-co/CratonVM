#[allow(unused_imports)]
use super::*;

/// A real JDK `MethodHandle` with none of this model's slots (a
/// `DirectMethodHandle`, a `BoundMethodHandle` species, ... that
/// [`mh_run_foreign_vmentry`] runs), for the `invoke` / `invokeExact` /
/// `invokeWithArguments` doors, which then treat it, as a downcall carrier
/// ([`mh_downcall_door_type`]), as a `STATIC` handle whose leaf descriptor is
/// its own `type`: `asType`'s refusals are judged against that type, the
/// arguments reach the dispatch as passed (it converts them), and the result
/// is boxed or widened from that type's return. The doors used to read this
/// model's absent slots as an unbound VIRTUAL handle of no descriptor: a
/// `null` first argument was the receiver's NPE, the call site lost its first
/// parameter, and the result went back unboxed (interpreter round i2 wave 54,
/// lane L4; `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`).
///
/// `None` with `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY=0`, for a handle with
/// this model's slots or downcall state, under `CRATONVM_PURE_BYTECODE` (whose
/// own real-handle lanes run first), and for `MethodHandleImpl$AsVarargsCollector`,
/// whose `asType` collects at another arity. One width compare for every other
/// handle.
pub fn mh_foreign_door_type(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if ctx.object_num_fields(mh) > MH_BOUND {
        return None;
    }
    mh_foreign_door_type_cold(ctx, mh)
}

#[cold]
#[inline(never)]
pub fn mh_foreign_door_type_cold(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if !crate::nbflags().mh_foreign_handle_runs_vmentry
        || cratonvm_native_api::unretire::pure_bytecode()
        || crate::panama::is_downcall_handle(&*ctx, mh)
    {
        return None;
    }
    let handle_class = ctx.class_id_by_name("java/lang/invoke/MethodHandle")?;
    let cid = ctx.class_id_of_object(mh);
    if !ctx.is_subclass(cid, handle_class)
        || ctx.class_name_arc_of_id(cid).as_deref()
            == Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
    {
        return None;
    }
    mh_declared_descriptor(ctx, mh)
}

/// Does an adapter native (`asType`, `bindTo`) hand this receiver to the
/// JDK's own body? A real JDK `MethodHandle` with none of this model's slots,
/// not a downcall carrier, outside `CRATONVM_PURE_BYTECODE`, with
/// `CRATONVM_MH_FOREIGN_ADAPTERS_RUN_JDK` on (interpreter round i2 wave 54,
/// lane L4). One width compare for every other receiver.
pub fn mh_foreign_adapts_through_jdk(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    ctx.object_num_fields(this) <= MH_BOUND && mh_foreign_adapts_through_jdk_cold(ctx, this)
}

#[cold]
#[inline(never)]
pub fn mh_foreign_adapts_through_jdk_cold(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    if !crate::nbflags().mh_foreign_adapters_run_jdk
        || cratonvm_native_api::unretire::pure_bytecode()
        || crate::panama::is_downcall_handle(&*ctx, this)
    {
        return false;
    }
    ctx.class_id_by_name("java/lang/invoke/MethodHandle")
        .is_some_and(|handle_class| ctx.is_subclass(ctx.class_id_of_object(this), handle_class))
}

/// MethodType equality is Class identity, including hidden classes and loader
/// identity. Rendering a synthetic lambda mirror falls back to Object, so a
/// descriptor-string shortcut can silently discard a required adaptation.
pub fn method_types_same_classes(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> bool {
    if a == b {
        return true;
    }
    let (
        Value::Object(Some(ar)),
        Value::Object(Some(br)),
        Value::Object(Some(ap)),
        Value::Object(Some(bp)),
    ) = (
        ctx.get_field_by_name(a, "rtype"),
        ctx.get_field_by_name(b, "rtype"),
        ctx.get_field_by_name(a, "ptypes"),
        ctx.get_field_by_name(b, "ptypes"),
    )
    else {
        return false;
    };
    if ar != br || ctx.array_length(ap) != ctx.array_length(bp) {
        return false;
    }
    (0..ctx.array_length(ap)).all(|i| {
        matches!(
        (ctx.get_array_element(ap, i), ctx.get_array_element(bp, i)),
        (Value::Object(Some(x)), Value::Object(Some(y))) if x == y)
    })
}

/// `asType(newType)` of a real JDK handle ([`mh_foreign_adapts_through_jdk`]):
/// the receiver itself for its own type, else the JDK's own
/// `MethodHandle.asType` bytecode (`asTypeUncached` -> `makePairwiseConvert`,
/// a new handle). `None` for any other receiver.
///
/// This model's `asType` copies a handle's slots and stamps the new `type`
/// on the copy; a real handle has no slots to copy (`mh_clone_handle` answers
/// `None`), so `mh_with_stamped_type` stamped it IN PLACE: the JDK's cached
/// handle (`MethodType.invokers()`' invokers are shared) took the new type
/// with its old form, and every later caller saw the wrong `type()`
/// (`L4W54RealJdkHandleArgs` rows `astype-*`). An `AsVarargsCollector` is
/// included: the JDK's own body is the one that collects.
pub fn mh_foreign_as_type(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallResult> {
    if !mh_foreign_adapts_through_jdk(ctx, this) {
        return None;
    }
    if matches!(ctx.get_field_by_name(this, "type"), Value::Object(Some(old))
        if method_types_same_classes(&*ctx, old, new_type))
    {
        return Some(Ok(Some(Value::Object(Some(this)))));
    }
    if let Some(cached) = mh_foreign_as_type_cache_hit(ctx, this, new_type) {
        return Some(Ok(Some(Value::Object(Some(cached)))));
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] asType of {} through the JDK's own body",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default()
        );
    }
    Some(ctx.invoke_virtual_bytecode_only(
        this,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(new_type))],
    ))
}

/// Interpreter round i5 wave 114, lane L4 (`CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ`,
/// default off; page `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`):
/// the JDK's `asTypeCached` first test, read here -- `asTypeCache` non-null
/// and its `type` the requested `MethodType` by identity answers that
/// adapter. Pure reads, so the JDK body would answer the same object with no
/// side effect. `None` (the JDK's `asType` bytecode runs) for a miss, the
/// switch off, and the `asTypeSoftCache` arm (`SoftReference.get` updates the
/// reference's clock, so that arm stays the JDK's).
///
/// Why it pays: `MethodHandle.asType` carries its own registered `Intrinsic`
/// (this file), so `interpreter::execute`'s first-call JIT block seals it
/// `native_skip` (`calls-native-shadowed-method`) and never counts it. The
/// [`mh_foreign_as_type`] re-entry (`invoke_virtual_bytecode_only`) therefore
/// interprets the JDK body on every call: the w113 census read `asType` `cold`
/// on all 16 000 re-entries, 11.0 us each, once per `invokeWithArguments`
/// body.
pub fn mh_foreign_as_type_cache_hit(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: ObjectRef,
) -> Option<ObjectRef> {
    if !crate::nbflags().mh_foreign_as_type_cache_read {
        return None;
    }
    let Value::Object(Some(cached)) = ctx.get_field_by_name(this, "asTypeCache") else {
        return None;
    };
    match ctx.get_field_by_name(cached, "type") {
        Value::Object(Some(t)) if t == new_type => Some(cached),
        _ => None,
    }
}

/// The JDK's own `asType(new_type)` on a real handle, as the natives that
/// adapt one call it (`invoke_virtual_bytecode_only`). Under
/// `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ` the body's two pure-read answers
/// come first: `newType == type` answers the handle itself, and an
/// `asTypeCache` hit answers the cached adapter
/// ([`mh_foreign_as_type_cache_hit`]). Interpreter round i5 wave 114, lane L4.
pub fn mh_real_as_type_bytecode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: ObjectRef,
) -> MethodCallResult {
    if crate::nbflags().mh_foreign_as_type_cache_read {
        if matches!(ctx.get_field_by_name(this, "type"), Value::Object(Some(t)) if t == new_type) {
            return Ok(Some(Value::Object(Some(this))));
        }
        if let Some(cached) = mh_foreign_as_type_cache_hit(ctx, this, new_type) {
            return Ok(Some(Value::Object(Some(cached))));
        }
    }
    ctx.invoke_virtual_bytecode_only(
        this,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(new_type))],
    )
}

/// `bindTo(x)` of a real JDK handle ([`mh_foreign_adapts_through_jdk`]): the
/// JDK's own body (`leadingReferenceParameter().cast(x)`, then
/// `bindArgumentL(0, x)`, a new `BoundMethodHandle`), where this model's
/// `bindTo` read the receiver's absent slots. `None` for any other receiver.
pub fn mh_foreign_bind_to(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    value: Value,
) -> Option<MethodCallResult> {
    if !mh_foreign_adapts_through_jdk(ctx, this) {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] bindTo of {} through the JDK's own body",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default()
        );
    }
    Some(ctx.invoke_virtual_bytecode_only(
        this,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        &[value],
    ))
}

/// The rest of the adapter natives over a real JDK handle
/// ([`mh_foreign_adapts_through_jdk`], and `CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK`):
/// `asVarargsCollector`, `withVarargs`, `asCollector` and `asSpreader` (both
/// arities) answer the JDK's own body (`MethodHandleImpl.makeVarargsCollector`,
/// `rebind().editor()...`, a new handle). This model's natives read the
/// receiver's absent slots: `asVarargsCollector`'s width-guarded marking was a
/// no-op, so the answer was the receiver itself, which does not collect; the
/// collector and spreader minted a carrier over a handle of no descriptor.
/// `args[0]` is the receiver, the rest the method's own arguments. `None` for
/// any other receiver. Interpreter round i2 wave 55, lane L4 (proposal
/// `i54-L4-proposal-every-adapter-native-hands-a-real-handle-to-the-jdk`).
/// `asFixedArity` / `isVarargsCollector` need no door: the JDK's base answers
/// (`this`, `false`) are this model's for a slotless receiver, and a real
/// `AsVarargsCollector` declares both itself, so its own bytecode runs.
pub fn mh_foreign_adapter_through_jdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    desc: &str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return None;
    };
    if ctx.object_num_fields(this) > MH_BOUND {
        return None;
    }
    mh_foreign_adapter_through_jdk_cold(ctx, this, &args[1..], name, desc)
}

#[cold]
#[inline(never)]
pub fn mh_foreign_adapter_through_jdk_cold(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    rest: &[Value],
    name: &str,
    desc: &str,
) -> Option<MethodCallResult> {
    // `asCollector` / `asSpreader` have their own switch: the JDK's bodies
    // answer a `BoundMethodHandle` over a spun `LambdaForm`, which this VM
    // cannot run outside `CRATONVM_PURE_BYTECODE` (the held
    // `InvokerBytecodeGenerator` natives fabricate its entry), where this
    // model's carrier over the real handle runs (measured wave 55,
    // `L4W55RealJdkAdapters` rows `collector-invoke`, `spreader-invoke`).
    let on = if matches!(name, "asCollector" | "asSpreader") {
        crate::nbflags().mh_foreign_collectors_run_jdk
    } else {
        crate::nbflags().mh_foreign_varargs_run_jdk
    };
    if !on || !mh_foreign_adapts_through_jdk(ctx, this) {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] {name} of {} through the JDK's own body",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default()
        );
    }
    // JIT round 21 lane reflect (correctness, unconditional; DEPTH21-3 of
    // `jit-r21-depth-proposals.md`, audit
    // `jr21-reflect-own-body-virtual-fallbacks-FIXED-20261015.md`): `MethodHandle`'s
    // OWN body, by `invokespecial`. These natives are registered on
    // `MethodHandle`'s own triples, so they run only where that method is the
    // one selected -- a receiver that does not override it, or an override's
    // `super.m()`. A virtual re-selection sent the second case straight back
    // into the override: JDK 25's `MethodHandleImpl$IntrinsicMethodHandle`
    // overrides `asCollector(Class,int)` and calls `super.asCollector` for
    // every intrinsic but `IDENTITY` (`MethodHandles.arrayElementGetter(..)
    // .asCollector(..)`), which then recursed until `StackOverflowError` under
    // `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK=1` (opt-in), wherever this
    // `Bridge` row serves the call, where HotSpot answers a handle. Every
    // other receiver reaching here selected `MethodHandle`'s own method, so
    // its answer is unchanged.
    let mut call = Vec::with_capacity(rest.len() + 1);
    call.push(Value::Object(Some(this)));
    call.extend_from_slice(rest);
    Some(ctx.invoke_special_bytecode_only("java/lang/invoke/MethodHandle", name, desc, &call))
}

/// `MH_KIND` of a handle with this model's slots, `-1` (no kind) for any other.
pub fn mh_kind_width_guarded(ctx: &dyn NativeContext, mh: ObjectRef) -> i32 {
    if ctx.object_num_fields(mh) <= MH_BOUND {
        return -1;
    }
    match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => -1,
    }
}

/// `MH_KIND` as the doors default it (`MH_KIND_VIRTUAL` when absent), without
/// reading past a real JDK handle's last slot (a heap OOB warning per call;
/// interpreter round i2 wave 56, lane L4: a real `AsVarargsCollector`, which
/// [`mh_foreign_door_type`] leaves to the doors, reached them).
pub fn mh_kind_or_virtual(ctx: &dyn NativeContext, mh: ObjectRef) -> i32 {
    match mh_kind_width_guarded(ctx, mh) {
        -1 => MH_KIND_VIRTUAL,
        k => k,
    }
}

/// Does `mh` carry a bound value in `MH_BOUND`? `false` for a handle without
/// this model's slots (see [`mh_kind_or_virtual`]).
pub fn mh_has_bound_guarded(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    ctx.object_num_fields(mh) > MH_BOUND
        && matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)))
}

/// The `(MH_DESC, MH_KIND)` the `invokeWithArguments` doors read, with a
/// downcall carrier read as [`mh_downcall_door_type`] says, and a real JDK
/// handle as [`mh_foreign_door_type`] says.
pub fn mh_door_desc_and_kind(ctx: &mut dyn NativeContext, mh: ObjectRef) -> (String, i32) {
    if let Some(own) = mh_downcall_door_type(ctx, mh) {
        return (own, MH_KIND_STATIC);
    }
    if let Some(own) = mh_foreign_door_type(ctx, mh) {
        return (own, MH_KIND_STATIC);
    }
    let desc = mh_read_desc(ctx, mh).unwrap_or_default();
    (desc, mh_kind_or_virtual(&*ctx, mh))
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): does this file's door do
/// anything to a downcall carrier's `invokeExact` beyond `mh_dispatch`'s
/// downcall arm, so the JIT's bound downcall row
/// (`panama::jit_downcall_bind_target`) must refuse it? `true` for an `asType`
/// copy (a recorded leaf descriptor, [`MH_DOWNCALL_LEAF_DESC`]: the door
/// converts its arguments and result) and for any marking bit (a collector, an
/// explicit-cast stamp, a cast chain, an exact invoker: `mh_dispatch` judges
/// those). Reads only.
pub fn mh_downcall_bind_screens_refuse(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    let adapted_copy = ctx.object_num_fields(mh) > MH_DOWNCALL_LEAF_DESC
        && matches!(
            ctx.get_field(mh, MH_DOWNCALL_LEAF_DESC),
            Value::Object(Some(_))
        );
    adapted_copy || mh_marking_bits(ctx, mh) != 0
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): the handle's declared `type`
/// rendered, as the `invokeExact` door's `exact_call_site_refusal` compares it
/// with the call site ([`mh_declared_descriptor`]).
pub fn mh_downcall_bind_declared_type(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<String> {
    mh_declared_descriptor(ctx, mh)
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): is an entry call site armed
/// on this thread ([`MH_ENTRY_CALL_SITE`])? The next `mh_dispatch` takes it, so
/// a bound downcall call that finds one runs `mh_dispatch` as the door would.
pub fn mh_entry_call_site_armed() -> bool {
    MH_ENTRY_CALL_SITE.with(|f| f.borrow().is_some())
}

/// The downcall arm's call when there is something to convert, or `None` (the
/// plain `invokeExact` of an unadapted carrier, which the arm makes as before).
/// `entry_site` is the call site an `invoke` / `invokeWithArguments` door armed.
///
/// The steps are the static leaf arm's: the entry adaptation to the handle's
/// `type` (its arity `WrongMethodTypeException` and casts, [`mh_entry_adapt`]),
/// then [`adapt_leaf_args`] to the C function's own descriptor (unboxing,
/// widening, the `null`-to-primitive `NullPointerException`), the call, and for
/// an `asType` copy the result converted from the C function's return to the
/// copy's ([`mh_downcall_outer_return`]). `mh` is pinned across the
/// conversions (a refusal, a box) and re-read for the call.
pub fn mh_downcall_adapted_call(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra: &[Value],
    entry_site: Option<&str>,
) -> Option<MethodCallResult> {
    let recorded = mh_downcall_leaf_desc(&*ctx, mh);
    if entry_site.is_none() && recorded.is_none() {
        return None;
    }
    let outer = mh_declared_descriptor(ctx, mh)?;
    let leaf = recorded.unwrap_or_else(|| outer.clone());
    let mh_pin = ctx.pin_native_root(mh);
    let answer = mh_downcall_adapted_call_pinned(ctx, mh_pin, mh, extra, entry_site, &outer, &leaf);
    ctx.unpin_native_roots(mh_pin);
    Some(answer)
}

/// The body of [`mh_downcall_adapted_call`], `mh` rooted by `mh_pin`.
pub fn mh_downcall_adapted_call_pinned(
    ctx: &mut dyn NativeContext,
    mh_pin: usize,
    mh: ObjectRef,
    extra: &[Value],
    entry_site: Option<&str>,
    outer: &str,
    leaf: &str,
) -> MethodCallResult {
    let full = match entry_site {
        Some(site) => mh_entry_adapt(ctx, mh, site, outer, false, extra)?,
        None => extra.to_vec(),
    };
    let site = leaf_site_params(Some(entry_site.unwrap_or(outer)), 0, full.len());
    let adapted = adapt_leaf_args(ctx, &full, leaf, site.as_deref(), false)?;
    let mh = ctx.read_native_pin(mh_pin, mh);
    let mut args = Vec::with_capacity(adapted.len() + 1);
    args.push(Value::Object(Some(mh)));
    args.extend_from_slice(&adapted);
    let result = crate::panama::pe_downcall_invoke(ctx, &args);
    mh_downcall_outer_return(ctx, result, leaf, outer)
}

/// The C function's result (`leaf`'s return) as the handle's `type` returns it
/// (`outer`): unchanged when the two agree (an unadapted carrier); a primitive
/// widened or narrowed as `asType` converts it, a value boxed for a reference
/// return, a `void` answered `null` ([`box_return_against_target`]).
pub fn mh_downcall_outer_return(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    leaf: &str,
    outer: &str,
) -> MethodCallResult {
    let (from, to) = (return_type_desc(leaf), return_type_desc(outer));
    if from == to {
        return result;
    }
    let (Some(f), Some(t)) = (from.bytes().next(), to.bytes().next()) else {
        return result;
    };
    match result {
        Ok(Some(v)) if !matches!(v, Value::Object(_)) => {
            match widen_return_value(v, f, t).or_else(|| narrow_return_value(v, f, t)) {
                Some(converted) => Ok(Some(converted)),
                // `byte` / `short` / `char` / `int` share `Value::Int`: a
                // widening between them converts nothing.
                None if b"ZBCSIJFD".contains(&t) => Ok(Some(v)),
                None => box_return_against_target(ctx, Ok(Some(v)), leaf, Some(outer)),
            }
        }
        other => box_return_against_target(ctx, other, leaf, Some(outer)),
    }
}

/// `asType(newType)` of a downcall carrier (`mt` non-null): the carrier itself
/// for its own type (HotSpot's `newType == type` line), `asType`'s
/// `WrongMethodTypeException` for a type it does not convert to
/// ([`mh_astype_refusal`]), otherwise a copy ([`mh_clone_handle`]: every slot,
/// the downcall state included) whose `type` is `mt` and which records the C
/// function's own descriptor in [`MH_DOWNCALL_LEAF_DESC`] (kept from the
/// receiver when it is itself a copy). A type that does not read keeps the old
/// passthrough.
pub fn mh_downcall_as_type(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mt: ObjectRef,
) -> MethodCallResult {
    let unchanged = Ok(Some(Value::Object(Some(this))));
    let (Some(old), Some(new)) = (
        mh_declared_descriptor(ctx, this),
        methodtype_to_descriptor(ctx, mt),
    ) else {
        return unchanged;
    };
    if old == new {
        return unchanged;
    }
    if let Some(refusal) = mh_astype_refusal(ctx, this, mt) {
        return Err(refusal);
    }
    let leaf = mh_downcall_leaf_desc(&*ctx, this).unwrap_or(old);
    let mt_pin = ctx.pin_native_root(mt);
    let Some(copy) = mh_clone_handle(ctx, this) else {
        ctx.unpin_native_roots(mt_pin);
        return unchanged;
    };
    let copy_pin = ctx.pin_native_root(copy);
    let text = ctx.create_string_uninterned(&leaf);
    let copy = ctx.read_native_pin(copy_pin, copy);
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field_by_name(copy, "type", Value::Object(Some(mt)));
    ctx.set_field(copy, MH_DOWNCALL_LEAF_DESC, Value::Object(Some(text)));
    ctx.unpin_native_roots(mt_pin);
    Ok(Some(Value::Object(Some(copy))))
}

/// The `WrongMethodTypeException` `mh.asType(newType)` must raise, or `None`
/// when the conversion is one HotSpot performs.
///
/// Every way of not knowing is an ACCEPT. The check needs four things to hold
/// before it will refuse — the receiver's `type` field must be a real
/// `MethodType`, both descriptors must parse, the raw bytecode descriptor must
/// agree that the conversion is impossible, and both signatures must render —
/// and if any of them fails this returns `None` and the passthrough proceeds
/// exactly as it did before. A missing refusal is the state this VM was already
/// in; a refusal HotSpot does not issue would break working `invokedynamic`
/// call sites, and there is no ordering of those two errors in which the second
/// is the better one.
///
/// The raw-descriptor step is the one that is not obvious: see the block
/// comment on the `asType` registration for why this body's own type mutation
/// makes a second `asType` on the same reference look unconvertible when
/// HotSpot, which hands out a fresh handle each time, would still be looking at
/// the original signature.
///
/// The message is HotSpot's, transcribed from
/// `MethodHandle.asTypeUncached` — `"cannot convert " + this + " to " + newType`
/// where `MethodHandle.toString()` is the literal `MethodHandle` followed
/// immediately by its `MethodType`, e.g.
/// `cannot convert MethodHandle(String)String to (int,int)int`. There is no
/// space after `MethodHandle`, and both signatures use simple type names.
/// Can a VARIABLE-ARITY handle of type `old_desc` be `asType`-adapted to
/// `new_desc` by collecting trailing arguments?
///
/// The JDK's rule (`MethodHandle.asVarargsCollector`'s contract, implemented in
/// `AsVarargsCollector.asTypeUncached`): the trailing array parameter absorbs
/// `newArity - collectArg` arguments, where `collectArg` is the index of that
/// array — so any requested arity from `collectArg` upwards is legal, including
/// `collectArg` itself, which collects zero into an empty array. Element-type
/// compatibility is left to dispatch, exactly as for the non-collecting path:
/// this predicate only decides whether the ARITY difference is a refusal, which
/// is the only thing `method_type_is_convertible_to` was rejecting.
pub fn varargs_astype_is_legal(old_desc: &str, new_desc: &str) -> bool {
    let (Some((old_params, _)), Some((new_params, _))) = (
        split_descriptor_params(old_desc),
        split_descriptor_params(new_desc),
    ) else {
        return false;
    };
    match old_params.last() {
        Some(last) if last.starts_with('[') => new_params.len() + 1 >= old_params.len(),
        _ => false,
    }
}

/// `CRATONVM_MH_LEAF_DESC` (interpreter round i6 wave 123, lane L4; proposal
/// `i42-L4-proposal-native-adapters-carry-hotspots-type`): is `mh`'s `type`
/// authoritative, so a raw-descriptor escape (an accept because `MH_DESC`
/// converts though `type` does not) must not be taken?
///
/// The escapes exist for the aliasing of an in-place `asType` stamp (the
/// block comment on the `asType` registration). Since `mh_with_stamped_type`
/// COPIES every handle wider than [`MH_VARARGS`] ([`mh_clone_handle`]), such a
/// handle's `type` is its own, and every native combinator mints its adapter's
/// `type` by the JDK's rule from its inputs' `type`s (`MH_DESC` keeping the
/// leaf / dispatch descriptor: the page's `MH_LEAF_DESC`). A narrower handle
/// is stamped in place and keeps the escape. `false` (take the escape, as
/// before) whenever the switch is off. Prints `[MH_LEAF_DESC]` under
/// `CRATONVM_DBG_MH_DISPATCH=1` for each escape it declines: the census of
/// handles whose `type` still differs from what dispatch reads.
pub fn mh_leaf_desc_type_is_authoritative(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    what: &str,
    type_desc: &str,
    raw_desc: &str,
    new_desc: &str,
) -> bool {
    if !crate::nbflags().mh_leaf_desc || ctx.object_num_fields(mh) <= MH_VARARGS {
        return false;
    }
    if crate::nbflags().dbg_mh_dispatch {
        let kind = match ctx.get_field(mh, MH_KIND) {
            Value::Int(k) => k,
            _ => -1,
        };
        eprintln!(
            "[MH_LEAF_DESC] {what}: kind {kind} type {type_desc} (MH_DESC {raw_desc}) to {new_desc}: refused on type"
        );
    }
    true
}

pub fn mh_astype_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallFailed> {
    // A real `MethodType` receiver only. `mh_type_descriptor` would fall back
    // to `MH_DESC`, which is the UNADAPTED signature — refusing on it would
    // refuse adapters that are already legal.
    let old_desc = match ctx.get_field_by_name(mh, "type") {
        Value::Object(Some(mt)) => methodtype_to_descriptor(ctx, mt)?,
        _ => return None,
    };
    let new_desc = methodtype_to_descriptor(ctx, new_type)?;
    mh_astype_refusal_desc(ctx, mh, &old_desc, &new_desc)
}

/// [`mh_astype_refusal`] on descriptors already read: `old_desc` is `mh`'s
/// declared `type`. Also the check a plain invoker makes of its target
/// (`Invokers.checkGenericType`'s `asType(t)`, wave 28).
pub fn mh_astype_refusal_desc(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    old_desc: &str,
    new_desc: &str,
) -> Option<MethodCallFailed> {
    if method_type_is_convertible_to(old_desc, new_desc)? {
        return None;
    }
    // A VARIABLE-ARITY handle adapts across arity, which is the whole point of
    // being one: HotSpot's `AsVarargsCollector.asTypeUncached` reacts to a
    // longer requested type by building `asCollector(arrayType, newArity -
    // collectArg)` and adapting THAT. `method_type_is_convertible_to` compares
    // arities and so refuses every one of those, which is what made
    // `findStatic(C, "m", (String,String[])String).asType((String,String,
    // String)String)` throw where HotSpot collects (`probes/
    // VarargsCollectorProbe.java`). Dispatch already performs the collection —
    // see `collect_trailing_varargs`, whose `ACC_VARARGS` trigger is the same
    // fact the marking records — so accepting here is all that was missing.
    if mh_is_varargs_collector(ctx, mh) && varargs_astype_is_legal(old_desc, new_desc) {
        return None;
    }
    // Round 15 wave 6 (lane mh5, MH4-1): a collector asked for fewer than
    // `collectArg` parameters fails in `AsVarargsCollector.asTypeUncached`'s
    // `asCollector(arrayType, newArity - collectArg)` (a negative length's
    // `IllegalArgumentException`), which it rethrows as this message.
    if mh_is_varargs_collector(ctx, mh)
        && split_descriptor_params(old_desc)
            .is_some_and(|(params, _)| params.last().is_some_and(|p| p.starts_with('[')))
        && mh_astype_fixed_collector_enabled()
    {
        return Some(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/invoke/WrongMethodTypeException",
            "cannot build collector",
        ));
    }
    // Our own aliasing, not a conversion HotSpot refuses.
    if let Some(raw) = mh_read_desc(ctx, mh) {
        // Round 15 wave 4 (lane mh3): a BOUND static handle's `MH_DESC` still
        // names the parameter `bindTo` captured, one more than the caller
        // passes, so `bindTo("-")` of `(String,String[])String` "converted" to
        // `(String,String)String` here; its caller-side raw type drops it.
        // Round 15 wave 6 (lane mh5, MH3-6): every kind whose `MH_DESC` names
        // its capture (`bound_capture_raw_desc`), not only a static one.
        let raw = bound_capture_raw_desc(ctx, mh, &raw).unwrap_or(raw);
        if method_type_is_convertible_to(&raw, new_desc) == Some(true) {
            // `CRATONVM_MH_LEAF_DESC` (interpreter round i6 wave 123, lane
            // L4): a copyable handle's `type` is its own, so the escape is
            // not taken and the refusal is HotSpot's.
            if !mh_leaf_desc_type_is_authoritative(ctx, mh, "asType", old_desc, &raw, new_desc) {
                return None;
            }
        }
    }
    let old_shown = method_type_display_in(&*ctx, old_desc)?;
    let new_shown = method_type_display_in(&*ctx, new_desc)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{old_shown} to {new_shown}"),
    ))
}

/// The `WrongMethodTypeException`
/// `MethodHandles.explicitCastArguments(target, newType)` must raise, or `None`.
///
/// Same shape and the same "every way of not knowing is an accept" rule as
/// [`mh_astype_refusal`], and the same reason for the raw-descriptor escape —
/// but the PREDICATE is different, and deliberately so: parameter count and
/// nothing else. See the registration's comment for the measurement.
pub fn mh_explicit_cast_refusal(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallFailed> {
    let old_desc = match ctx.get_field_by_name(target, "type") {
        Value::Object(Some(mt)) => methodtype_to_descriptor(ctx, mt)?,
        _ => return None,
    };
    let new_desc = methodtype_to_descriptor(ctx, new_type)?;
    let (old_params, _) = split_descriptor_params(&old_desc)?;
    let (new_params, _) = split_descriptor_params(&new_desc)?;
    if old_params.len() == new_params.len() {
        return None;
    }
    if let Some(raw) = mh_read_desc(ctx, target) {
        // Round 15 wave 6 (lane mh5, MH3-6): a bound handle's `MH_DESC` that
        // names its capture is one parameter longer than any call to it.
        let raw = bound_capture_raw_desc(ctx, target, &raw).unwrap_or(raw);
        if let Some((raw_params, _)) = split_descriptor_params(&raw) {
            if raw_params.len() == new_params.len()
                && !mh_leaf_desc_type_is_authoritative(
                    ctx,
                    target,
                    "explicitCastArguments",
                    &old_desc,
                    &raw,
                    &new_desc,
                )
            {
                return None;
            }
        }
    }
    let old_shown = method_type_display_in(&*ctx, &old_desc)?;
    let new_shown = method_type_display_in(&*ctx, &new_desc)?;
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        &format!("cannot explicitly cast MethodHandle{old_shown} to {new_shown}"),
    ))
}

/// WP2.9 — Robust class-name extraction that survives the descriptor-coercion
/// drift where `Class` mirrors store `Object(name_str)` in slot 1 but read
/// back as `Int(ptr_lo)` / `Long(ptr_bits)` because the field-coercion path
/// only handles Object/Int(0)/Long(0)/Double — not arbitrary integer
/// bit-patterns smuggled through compact storage.
///
/// Strategy:
///   1. Try the reverse map (mirror → ClassId via `class_id_from_mirror`),
///      then look up the name via `class_name_of_id`.
///   2. Fall back to `mirror_class_name` (slot-1 read) for primitive mirrors
///      and other cases the reverse map doesn't cover.
///   3. Last resort: try to reconstruct the slot-1 string pointer from the
///      smuggled bits (Long or Int(non-zero)) and read it as a string.
pub fn resolve_class_name_robust(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
) -> Option<String> {
    // 1. Reverse-map lookup (preferred — never hits the bad coercion path).
    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
        if let Some(name) = ctx.class_name_of_id(cid) {
            return Some(name);
        }
    }
    // 2. Standard slot-1 read (works for primitive mirrors and clean paths).
    if let Some(name) = crate::lang_class::mirror_class_name(ctx, mirror) {
        if !name.is_empty() {
            return Some(name);
        }
    }
    // 3. C20 — REMOVED: previously this fell back to reconstructing a
    // String `ObjectRef` from the slot-1 raw bits (Long or Int(non-zero))
    // after only an alignment+48-bit-range check. That was the
    // memory-safety footgun called out in review item #5: arbitrary
    // Java-controlled scalars satisfying the bit-pattern predicate would
    // be fed to `ctx.read_string(...)`, which dereferences the pointer to
    // read the header.
    //
    // `NativeContext` does not expose an `is_heap_addr` probe (the
    // primitive the C7 interpreter fix uses on `&shared.heap`), and we
    // have no reverse-map for arbitrary String objects analogous to
    // `class_id_from_mirror` for Class mirrors. The remaining safe
    // recovery is steps 1-2 above; if both miss, the right answer is
    // `None` rather than fabricating a wild reference.
    None
}

/// WP2.9 — Robust mirror-from-slot recovery: gets a Class mirror from a slot
/// that may have been compacted to Long/Int bits by the descriptor-coercion
/// path. Returns the original ObjectRef when slot value is Object, otherwise
/// reconstructs from heap-aligned 48-bit bit patterns.
///
/// C20 — heap-membership filter on Java-controlled `ObjectRef::from_raw`
/// reconstruction. Mirrors the `vm::interpreter::pop_object_ref_ctx_with`
/// C7 fix: alignment + 48-bit-range alone are not sufficient to prove a
/// Long/Int slot holds a smuggled mirror — an honest scalar value can
/// satisfy them too. Because `NativeContext` does not expose a raw
/// `is_heap_addr` probe, we use `ctx.class_id_from_mirror(candidate)` as
/// the heap-membership oracle: it consults the VM's
/// `class_mirrors_reverse` map (a hashmap lookup keyed on the
/// `ObjectRef`'s raw address — no dereference) and returns `Some(_)`
/// only when the candidate is a registered Class mirror. On miss we
/// return `None`, matching the function's existing "miss" idiom rather
/// than fabricating a wild `ObjectRef` for downstream `read_string` /
/// `heap_kind_of` calls to dereference.
pub fn recover_class_mirror_from_slot(
    ctx: &dyn NativeContext,
    value: Value,
) -> Option<ObjectRef> {
    match value {
        Value::Object(Some(o)) => Some(o),
        Value::Object(None) => None,
        Value::Long(l) => {
            let bits = l as u64;
            if bits != 0 && (bits & 0x7) == 0 && bits < (1u64 << 48) {
                let ptr = bits as usize as *mut u8;
                // SAFETY: the alignment+range pre-checks above narrow the
                // candidate to addresses our heap arenas could in principle
                // hand out. `ObjectRef::from_raw` is a typed wrapper that
                // does not dereference; the immediately-following
                // `class_id_from_mirror` lookup is a hashmap probe on the
                // raw address (no header read), so a wild candidate is
                // rejected without ever being touched as a pointer.
                let candidate = unsafe { ObjectRef::from_raw(ptr) };
                if ctx.class_id_from_mirror(candidate).is_some() {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                None
            }
        }
        Value::Int(i) if i != 0 => {
            // Lower 32 bits of a heap pointer.  Bit-aligned and within 48-bit
            // range checks both pass for heap-allocated ObjectRefs whose top
            // 16 bits are zero (true on 64-bit Windows for our arenas).
            let bits = i as u32 as u64;
            if (bits & 0x7) == 0 && bits != 0 && bits < (1u64 << 48) {
                let ptr = bits as usize as *mut u8;
                // SAFETY: see Long-arm comment above. Same `class_id_from_mirror`
                // hashmap probe gates the candidate before any heap access.
                let candidate = unsafe { ObjectRef::from_raw(ptr) };
                if ctx.class_id_from_mirror(candidate).is_some() {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Reconstruct a full JVM method descriptor from a MethodType object.
/// MethodType has field 0 = returnType (Class mirror), field 1 = paramTypes (Class[] or null).
pub fn descriptor_from_method_type(ctx: &dyn NativeContext, mt: ObjectRef) -> String {
    let mut desc = String::from("(");

    // Parameter types (field 1 = Class[] array or null)
    if let Value::Object(Some(params_arr)) = ctx.get_field(mt, 1) {
        let len = ctx.array_length(params_arr);
        for i in 0..len {
            if let Some(param_mirror) =
                recover_class_mirror_from_slot(ctx, ctx.get_array_element(params_arr, i))
            {
                desc.push_str(&mirror_to_descriptor(ctx, param_mirror));
            }
        }
    }
    desc.push(')');

    // Return type (field 0 = Class mirror) — recover from any slot encoding.
    if let Some(ret_mirror) = recover_class_mirror_from_slot(ctx, ctx.get_field(mt, 0)) {
        desc.push_str(&mirror_to_descriptor(ctx, ret_mirror));    } else {
        desc.push('V'); // default void    
}

    desc}

/// Build a field descriptor for a single Class mirror (used by findGetter/findSetter).
pub fn field_descriptor_from_mirror(
    ctx: &dyn NativeContext,
    type_mirror: ObjectRef,
) -> Cow<'static, str> {
    mirror_to_descriptor(ctx, type_mirror)
}

// ---------------------------------------------------------------------------
// `MethodType` interning
// ---------------------------------------------------------------------------
//
// `MethodType` is specified to be INTERNED: `MethodType.methodType(...)`,
// `fromMethodDescriptorString(...)` and an `ldc CONSTANT_MethodType` must all
// hand back the *same instance* for the same descriptor, so `==` holds and
// `MethodHandle` call-site matching stays identity-based (JVMS §5.4.3.5,
// `java.lang.invoke.MethodType`'s class javadoc).
//
// The four `methodType` registrations below are `Bridge` natives that SHADOW
// the real JDK bytecode — the `--real-jdk` census reports them as
// `bridge-ran-over-bytecode` — and each one minted a fresh 2-field carrier.
// So on a real image every `methodType()` result was a NEW object:
//
// ```text
//   probes/PMt.java, --real-jdk, one binary          CratonVM   HotSpot 25
//     MethodType.methodType(String,String) == itself  false      true
//     fromMethodDescriptorString == itself            true       true   <- interns
//     fromMethodDescriptorString == methodType(..)    false      true
// ```
//
// which is the `RJdkProxyIface.ldcMethodType` failure: the `ldc` decoder
// (`vm/src/runtime/interpreter/constants.rs`) already does the right thing and
// prefers the real interning factory — the NON-interned side of that `==` was
// the fixture's right-hand `MethodType.methodType(String.class, String.class)`,
// i.e. this file.
//
// Retiring the four shadows (re-tagging them `SyntheticStub` so `--real-jdk`
// refuses them, as `native-api/src/retired_shadow.rs` does for JUL) is the
// structurally right cure and is NOMINATED — it cannot be done here, because
// four rows moving `Bridge` -> `SyntheticStub` also move three frozen
// baselines that can only be re-frozen from a Linux/JDK-25 census
// (`the native-registry census` runs `SLACK = 0`). What this file can do — in BOTH modes,
// with no census motion — is stop fabricating and delegate to the JDK's own
// interning factories.

/// Class name of the interning factory chain's receiver.
pub const MT_CLASS: &str = "java/lang/invoke/MethodType";
/// `MethodType.genericMethodType(int)` — an interned all-`Object` type. Used as
/// the CHAIN BASE because it takes no strings and no `ClassLoader`: the JDK
/// answers `genericMethodType(0)` out of its own `objectOnlyTypes` cache, so
/// there is no descriptor to parse and no class name to resolve.
pub const MT_GENERIC: &str = "(I)Ljava/lang/invoke/MethodType;";
/// `MethodType.changeReturnType(Class)`.
pub const MT_CHANGE_RETURN: &str = "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;";
/// `MethodType.insertParameterTypes(int, Class...)`. Called with index 0 on the
/// empty base rather than `appendParameterTypes`, which would route through the
/// `parameterCount()` shim below for no reason.
pub const MT_INSERT_PARAMS: &str = "(I[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;";

/// The real JDK's interned `MethodType` for `rtype`/`ptypes`, or `None` when
/// the class library does not carry the factory chain (synthetic-JDK builds),
/// in which case the caller falls back to the 2-field carrier.
///
/// **Why this chain and not `fromMethodDescriptorString`.** That factory is
/// what the `ldc` decoder and `StackFrame.getMethodType()` use, and it interns
/// — but it takes a *descriptor string plus a `ClassLoader`*, and the caller
/// here holds `Class` MIRRORS, whose loaders may differ from each other. Going
/// through a descriptor would re-resolve every parameter by NAME under one
/// guessed loader, which is both slower and wrong the moment two loaders each
/// define a class of the same name. `genericMethodType(0)` ->
/// `insertParameterTypes(0, ptypes)` -> `changeReturnType(rtype)` consumes the
/// mirrors as they are: no strings, no loader, no name resolution, and every
/// step lands in `MethodType.makeImpl(..., true)`, i.e. the intern table.
///
/// Verified end to end on the SHIPPING binary before the change, by running
/// that exact sequence from Java (probes/PMt2.java): under `--real-jdk` the
/// composed type is `==` to `fromMethodDescriptorString`'s for `(String)String`
/// and `()void`, is stable across calls, and answers `returnType`/
/// `parameterType(i)` with the identical mirrors — so what is being delegated
/// to is known to work in this VM, not assumed to.
///
/// None of the three methods is registered anywhere in this tree, so invoking
/// them reaches real bytecode instead of re-entering this file.
pub fn method_type_interned_via_jdk(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if !ctx.method_exists(MT_CLASS, "genericMethodType", MT_GENERIC)
        || !ctx.method_exists(MT_CLASS, "changeReturnType", MT_CHANGE_RETURN)
        || !ctx.method_exists(MT_CLASS, "insertParameterTypes", MT_INSERT_PARAMS)
    {
        return Ok(None);
    }
    // GC-safety: every `invoke*` below can collect and relocate the caller's
    // mirrors, which are bare `ObjectRef`s in Rust locals. Pin them for the
    // whole chain and re-read through the handles after each call.
    let rtype_pin = ctx.pin_native_root(rtype);
    let ptypes_pin = ptypes.map(|p| ctx.pin_native_root(p));
    let out = method_type_interned_chain(ctx, rtype, rtype_pin, ptypes, ptypes_pin);
    ctx.unpin_native_roots(rtype_pin);
    out
}

/// The pinned body of [`method_type_interned_via_jdk`].
///
/// Failure policy, following the same split
/// `vm/src/runtime/interpreter/constants.rs` established for the `ldc` path: a
/// Java-visible exception from `insertParameterTypes` / `changeReturnType` IS
/// the answer (`void` as a parameter type, a null element, too many argument
/// slots — HotSpot throws `IllegalArgumentException`/`NullPointerException`
/// there too), so it propagates rather than being replaced by a fabricated
/// type. Anything else — a missing return value, an internal error, or a
/// failure of the BASE call, which validates nothing and cannot be the user's
/// answer — falls back to the carrier this file has always built.
pub fn method_type_interned_chain(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    rtype_pin: usize,
    ptypes: Option<ObjectRef>,
    ptypes_pin: Option<usize>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let mut mt = match ctx.invoke(MT_CLASS, "genericMethodType", MT_GENERIC, &[Value::Int(0)]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Ok(None),
    };
    let mut mt_pin = ctx.pin_native_root(mt);
    if let (Some(p), Some(p_pin)) = (ptypes, ptypes_pin) {
        let arr = ctx.read_native_pin(p_pin, p);
        if ctx.array_length(arr) > 0 {
            let recv = ctx.read_native_pin(mt_pin, mt);
            match ctx.invoke_virtual(
                recv,
                "insertParameterTypes",
                MT_INSERT_PARAMS,
                &[Value::Int(0), Value::Object(Some(arr))],
            ) {
                Ok(Some(Value::Object(Some(o)))) => {
                    mt = o;
                    mt_pin = ctx.pin_native_root(mt);
                }
                Err(e @ MethodCallFailed::ExceptionThrown(_)) => return Err(e),
                _ => return Ok(None),
            }
        }
    }
    let recv = ctx.read_native_pin(mt_pin, mt);
    let rt = ctx.read_native_pin(rtype_pin, rtype);
    match ctx.invoke_virtual(
        recv,
        "changeReturnType",
        MT_CHANGE_RETURN,
        &[Value::Object(Some(rt))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => Ok(Some(o)),
        Err(e @ MethodCallFailed::ExceptionThrown(_)) => Err(e),
        _ => Ok(None),
    }
}

/// One `MethodType` for the four `methodType(...)` overloads: the real JDK's
/// interned instance when the image has the factory chain, otherwise the
/// 2-field `(returnType, parameterArray)` carrier this file has always minted,
/// with its fabricated `MethodTypeForm`.
///
/// `ptypes` is consumed as given; `None` (an absent or null parameter array)
/// becomes a 0-length one on the fallback path, so `parameterCount()` and
/// `parameterArray()` never meet a null slot.
/// A `Class[]` of `len` (i6-L2): the declared type of `MethodType.ptypes`
/// and `parameterArray()`, never an untyped `Object[]`. Same GC contract as
/// `new_array`: pin what is held across it.
pub fn new_class_array(ctx: &mut dyn NativeContext, len: usize) -> ObjectRef {
    let component = crate::lang_class::reflection_component_id(ctx, "java/lang/Class");
    ctx.new_ref_array(component, len)
}

pub fn make_method_type(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> MethodCallResult {
    if let Some(interned) = method_type_interned_via_jdk(ctx, rtype, ptypes)? {
        return Ok(Some(Value::Object(Some(interned))));
    }
    make_method_type_carrier(ctx, rtype, ptypes)
}

/// [`make_method_type`] for an UNTRUSTED `ptypes` array -- the caller's own
/// `Class[]` handed to `methodType(Class, Class[])` (interpreter round i3 wave
/// 79, lane L4; page
/// `i76-L4-synthetic-method-type-keeps-the-callers-parameter-array`): the
/// fallback carrier keeps a COPY, as HotSpot's `makeImpl(rtype, ptypes, false)`
/// clones an untrusted array. The carrier stored the caller's array itself, so
/// `a[0] = Integer.class` after `methodType(int.class, a)` changed the type.
/// Over a real JDK the factory chain answers first and copies on its own
/// (`insertParameterTypes`); only the fallback pays the copy.
pub fn make_method_type_copying(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> MethodCallResult {
    if let Some(interned) = method_type_interned_via_jdk(ctx, rtype, ptypes)? {
        return Ok(Some(Value::Object(Some(interned))));
    }
    let Some(params) = ptypes else {
        return make_method_type_carrier(ctx, rtype, None);
    };
    let n = ctx.array_length(params);
    // GC-safety: the copy's allocation can relocate both the mirror and the
    // caller's array.
    let rtype_pin = ctx.pin_native_root(rtype);
    let params_pin = ctx.pin_native_root(params);
    let copy = new_class_array(ctx, n);
    let copy_pin = ctx.pin_native_root(copy);
    let params = ctx.read_native_pin(params_pin, params);
    let copy = ctx.read_native_pin(copy_pin, copy);
    if !ctx.bulk_array_copy(params, 0, copy, 0, n) {
        for i in 0..n {
            let v = ctx.get_array_element(params, i);
            ctx.set_array_element(copy, i, v);
        }
    }
    let copy = ctx.read_native_pin(copy_pin, copy);
    let rtype = ctx.read_native_pin(rtype_pin, rtype);
    ctx.unpin_native_roots(rtype_pin);
    // Nothing collects between the unpin and the carrier's own pins.
    make_method_type_carrier(ctx, rtype, Some(copy))
}

/// The synthetic 2-field carrier [`make_method_type`] mints when the JDK's
/// factory chain does not answer (a synthetic class library), over `ptypes`
/// as given.
pub fn make_method_type_carrier(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> MethodCallResult {
    // GC-safety: the allocation and the empty-array creation below can each
    // relocate the caller's mirrors and the carrier itself.
    let rtype_pin = ctx.pin_native_root(rtype);
    let ptypes_pin = ptypes.map(|p| ctx.pin_native_root(p));
    let obj = try_alloc_concurrent_synthetic(ctx, MT_CLASS, 6)?;
    let obj_pin = ctx.pin_native_root(obj);
    let rt = ctx.read_native_pin(rtype_pin, rtype);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(rt)));
    let params = match (ptypes, ptypes_pin) {
        (Some(p), Some(pin)) => ctx.read_native_pin(pin, p),
        _ => new_class_array(ctx, 0),
    };
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 1, Value::Object(Some(params)));
    let mut obj = obj;
    populate_method_type_form(ctx, &mut obj)?;
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(rtype_pin);
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 76, lane L4: `--compatible`'s `MethodType.methodType`
// factories yield to the JDK's bytecode (`CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE`)
// ---------------------------------------------------------------------------
//
// Over a real JDK the four `methodType` `Bridge`s below build nothing of their
// own: `make_method_type` first asks `method_type_interned_via_jdk`, which runs
// `genericMethodType(0)` -> `insertParameterTypes(0, ptypes)` ->
// `changeReturnType(rtype)` from Rust and answers the JDK's interned instance.
// Per call that is a native crossing, three by-name `method_exists` probes, up
// to three native-to-Java re-entries and two intern probes, where the JDK's own
// body is ONE `makeImpl(rtype, ptypes, true)`. And a `null` component throws
// from the native (`obj_arg`) without the JDK's `makeImpl` frame (page
// `docs/feature-designs/native-dispatch-architecture.md`).
// Under `--real-jdk` the bridges already lose to the bytecode (they are rows
// of `RETIRED_SHADOW_SBR_MHLOOKUP_TRIPLES`), so behind the switch both modes
// run the same Java and answer the same objects. A synthetic JDK has no
// factory chain and keeps the bridges (and their 2-field carrier). The four
// are registered as `Bridge`s and the registry re-tags them `SyntheticStub`
// (retired shadows), so the switch only REMOVES stubs: the frozen baselines of
// `the native-registry census` are measured with it off (default)
// and are unchanged.
//
// Every Rust caller that needs a `MethodType` already goes through
// `make_method_type` / `method_type_interned_via_jdk` or
// `fromMethodDescriptorString` directly, never through these registrations.

/// `CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE` -- default OFF, `truthy_word`,
/// read at registration.
pub fn mh_compat_method_type_bytecode_enabled() -> bool {
    crate::nbflags().mh_compat_method_type_bytecode
}

/// Does `r`'s VM leave the four `methodType` factories to the JDK's bytecode?
/// The switch, over a real JDK.
pub fn mh_compat_method_type_bytecode_applies(r: &NativeMethodRegistry) -> bool {
    mh_compat_method_type_bytecode_enabled() && r.real_jdk()
}

/// The four `MethodType.methodType` overloads as `Bridge`s over
/// [`make_method_type`]. Sets its own category and restores the caller's.
pub fn register_method_type_factory_bridges(r: &mut NativeMethodRegistry, mt: &str) {
            r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let params = match args.get(1) {
                Some(Value::Object(p)) => *p,
                _ => None,
            };
            // The caller's array: copied by the fallback carrier (wave 79).
            make_method_type_copying(ctx, ret, params)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            make_method_type(ctx, ret, None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let param = obj_arg(args, 1)?;
            // GC-safety: `new_array` can collect and relocate both mirrors.
            let ret_pin = ctx.pin_native_root(ret);
            let param_pin = ctx.pin_native_root(param);
            let arr = new_class_array(ctx, 1);
            let param = ctx.read_native_pin(param_pin, param);
            ctx.set_array_element(arr, 0, Value::Object(Some(param)));
            let ret = ctx.read_native_pin(ret_pin, ret);
            let out = make_method_type(ctx, ret, Some(arr));
            ctx.unpin_native_roots(ret_pin);
            out
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // methodType(Class rtype, Class ptype0, Class... morePtypes)
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let ptype0 = obj_arg(args, 1)?;
            // `morePtypes` may be EMPTY; it may not be NULL. HotSpot's
            // `methodType(rtype, ptype0, ptypes)` runs the whole array through
            // `checkPtypes`, which dereferences it -- a null there is an NPE,
            // not "no more parameters". The comment this replaces said "may be
            // null or an array" and the code obliged, so
            // `methodType(int.class, String.class, (Class<?>[]) null)` built
            // `(String)int` where HotSpot throws.
            let more = match args.get(2) {
                Some(Value::Object(Some(a))) => Some(*a),
                Some(Value::Object(None)) => {
                    return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                        cratonvm_types::error::VmError::Runtime(
                            cratonvm_types::error::RuntimeError::NullPointerException {
                                message: Some(
                                    "MethodType.methodType: ptypes must not be null".to_string(),
                                ),
                            },
                        ),
                    ));                }
                _ => None,
            };
            let more_len = more.map(|a| ctx.array_length(a)).unwrap_or(0);
            // GC-safety: `new_array` and the element copies can each collect;
            // pin every reference the loop below still needs afterwards.
            let ret_pin = ctx.pin_native_root(ret);
            let ptype0_pin = ctx.pin_native_root(ptype0);
            let more_pin = more.map(|a| ctx.pin_native_root(a));
            let total = 1 + more_len;
            let arr = new_class_array(ctx, total);
            let arr_pin = ctx.pin_native_root(arr);
            let ptype0 = ctx.read_native_pin(ptype0_pin, ptype0);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, 0, Value::Object(Some(ptype0)));
            if let (Some(m), Some(m_pin)) = (more, more_pin) {
                for i in 0..more_len {
                    let m = ctx.read_native_pin(m_pin, m);
                    let elem = ctx.get_array_element(m, i);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, 1 + i, elem);
                }
            }
            let ret = ctx.read_native_pin(ret_pin, ret);
            let arr = ctx.read_native_pin(arr_pin, arr);
            let out = make_method_type(ctx, ret, Some(arr));
            ctx.unpin_native_roots(ret_pin);
            out
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// java.lang.invoke — MethodHandle, MethodType, MethodHandles (stubs)
// ---------------------------------------------------------------------------
pub fn register_phase54_method_handle(r: &mut NativeMethodRegistry) {
            // --- MethodType (2-field: returnType=0, paramTypes=1) ---
    let mt = "java/lang/invoke/MethodType";
    // Interpreter round i3 wave 76, lane L4
    // (`CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE`, default off): over a real JDK
    // the four `methodType` factories are left to the JDK's bytecode.
    if !mh_compat_method_type_bytecode_applies(r) {
        register_method_type_factory_bridges(r, mt);
    }
    r.register(mt, "returnType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(mt, "parameterCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 1) {
            Ok(Some(Value::Int(ctx.array_length(arr) as i32)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    // `parameterType(i)` is bounds-checked. HotSpot indexes its own `ptypes`
    // array and so raises `ArrayIndexOutOfBoundsException`; this used to cast a
    // NEGATIVE index straight to `usize` -- `-1` becoming 18 446 744 073 709
    // 551 615 -- and hand it to `get_array_element`, which answered a null the
    // caller then dereferenced, so the exception a caller saw was an NPE from
    // somewhere else entirely.
    r.register(mt, "parameterType", "(I)Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if let Value::Object(Some(arr)) = ctx.get_field(this, 1) {
            let len = ctx.array_length(arr) as i64;
            if i64::from(raw) < 0 || i64::from(raw) >= len {
                return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                            index: raw,
                            message: Some(format!("Index {raw} out of bounds for length {len}")),
                        },
                    ),
                ));
            }
            Ok(Some(ctx.get_array_element(arr, raw as usize)))
        } else {
            Ok(Some(Value::Object(None)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    // `MethodType.parameterArray()` returns a COPY. The JDK's whole body is
    // `return ptypes.clone();`, and the clone is the contract: `MethodType` is
    // specified immutable AND interned, so handing back the live `ptypes` lets
    // any caller rewrite a type that other callers already hold.
    //
    // MEASURED before this, against jdk-25.0.3.9-hotspot:
    //
    //   MethodType mt = methodType(int.class, String.class, long.class);
    //   mt.parameterArray()[0] = int.class;
    //   mt                        HotSpot (String,long)int   CratonVM (int,long)int
    //   mt.parameterType(0)               java.lang.String              int
    //
    // One write, and the type is a different type for good. It took six other
    // rows down with it in `probes/L8InvokeLookupSweep.java` -- `toString`,
    // `descriptorString`, `changeReturnType`, `appendParameterTypes`, `wrap`
    // and `unwrap` all read the corrupted array afterwards and were NOT six
    // more defects, which is why the probe asks `isCopy` BEFORE it asks any of
    // them.
    r.register(mt, "parameterArray", "()[Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let Value::Object(Some(params)) = ctx.get_field(this, 1) else {
            let empty = new_class_array(ctx, 0);
            return Ok(Some(Value::Object(Some(empty))));
        };
        let n = ctx.array_length(params);
        // GC-safety: `new_array` can collect and relocate `params`, so pin it
        // across the allocation and read it back before the copy.
        let params_pin = ctx.pin_native_root(params);
        let out = new_class_array(ctx, n);
        let out_pin = ctx.pin_native_root(out);
        let params = ctx.read_native_pin(params_pin, params);
        let out = ctx.read_native_pin(out_pin, out);
        if !ctx.bulk_array_copy(params, 0, out, 0, n) {
            for i in 0..n {
                let v = ctx.get_array_element(params, i);
                ctx.set_array_element(out, i, v);
            }
        }
        let out = ctx.read_native_pin(out_pin, out);
        ctx.unpin_native_roots(params_pin);
        Ok(Some(Value::Object(Some(out))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `Class.getSimpleName()`'s spelling of a binary class name.
    //
    // The last-segment-after-`.`-or-`$` rule this used to apply is right for a
    // plain class and wrong for every ARRAY class: an array's binary name is
    // `[Ljava/lang/String;` or `[I`, whose last segment is `String;` and `[I`,
    // where the JDK prints `String[]` and `int[]`. `MethodType.toString()` is
    // the message text of every `WrongMethodTypeException` and
    // `MethodHandle` linkage error, so the difference read as
    //
    // ```text
    //   HotSpot   (String,String[])String
    //   CratonVM  (String,String;)String
    // ```
    //
    // in a diagnostic whose entire job is to be compared, character for
    // character, against the type the caller asked for
    // (`probes/MhCombinatorProbe.java` / `probes/VarargsCollectorProbe.java`).
    fn class_display_simple_name(raw: &str) -> String {
        let name = raw.hidden_dotted();
        let dims = name.bytes().take_while(|b| *b == b'[').count();
        let base = &name[dims..];
        let suffix = "[]".repeat(dims);
        // A hidden class (`/0x<hex>` tail) is top-level to `getSimpleName()`:
        // `getName()` cut at its last `.` only (`Foo$$Lambda/0x12`), never at
        // a `$` (wave 15 review, rev-w14-native item 5).
        let simple_tail = |n: &str| -> String {
            if crate::lang_class::hidden_name_suffix_split(n).is_some() {
                n.rsplit('.').next().unwrap_or(n).to_string()
            } else {
                n.rsplit(['.', '$']).next().unwrap_or(n).to_string()
            }
        };
        if dims == 0 {
            return simple_tail(base);
        }
        match base.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            Some(inner) => {
                let tail = simple_tail(inner);
                format!("{tail}{suffix}")
            }
            // A primitive element type is a one-letter descriptor, not a name.
            None => {
                let elem = match base {
                    "B" => "byte",
                    "C" => "char",
                    "D" => "double",
                    "F" => "float",
                    "I" => "int",
                    "J" => "long",
                    "S" => "short",
                    "Z" => "boolean",
                    other => other,
                };
                format!("{elem}{suffix}")
            }
        }
    }

    // `MethodType.toString()` is specified as `(P1,P2,…)R` using each type's
    // SIMPLE name — `(Bean)int`, not `MethodType(1 params)`, which is what this
    // returned until 2026-08-04 and which no JDK ever prints.
    //
    // It matters beyond cosmetics: this string is what `WrongMethodTypeException`
    // and every `MethodHandle` linkage error carry as their message, so a
    // placeholder here turns a diagnosable "expected (Bean)int, found (int)int"
    // into two identical strings.
    //
    // The receiver may have either layout, so read the two fields by shape.
    // Synthetic `MethodType` is the 2-field `(returnType, parameterArray)` this
    // file mints; the real JDK class names them `rtype` / `ptypes` and has
    // several more fields behind them. Falling back to the placeholder when
    // neither read works keeps this total — a `toString` that raises would be a
    // worse failure than a vague one.
    r.register(mt, "toString", "()Ljava/lang/String;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let synthetic = ctx.object_num_fields(this) <= 2;
        let read_types = |ctx: &dyn cratonvm_native_api::NativeContext, this: ObjectRef| {
            if synthetic {
                (ctx.get_field(this, 0), ctx.get_field(this, 1))
            } else {
                (
                    ctx.get_field_by_name(this, "rtype"),
                    ctx.get_field_by_name(this, "ptypes"),
                )
            }
        };
        let (mut rtype, mut ptypes) = read_types(&*ctx, this);

        // JIT round 17 wave 8, lane mh7 (third pass): the JDK body asks
        // `getSimpleName()` of the return type, then each parameter type, so a
        // nested class whose outer class disagrees on `InnerClasses` throws
        // `IncompatibleClassChangeError` here (`R17Mh7Inner
        // methodtype-toString`, `--compatible`). `CRATONVM_CLASS_INNER_CLASSES_CHECK`.
        if crate::classloader::any_defining_loader_registered(crate::classloader::loader_tables(&*ctx)) {
            let mut ids: Vec<ClassId> = Vec::new();
            if let Value::Object(Some(r)) = rtype {
                ids.extend(mirror_class_id(&*ctx, r));
            }
            if let Value::Object(Some(arr)) = ptypes {
                for i in 0..ctx.array_length(arr) {
                    if let Value::Object(Some(m)) = ctx.get_array_element(arr, i) {
                        ids.extend(mirror_class_id(&*ctx, m));
                    }
                }
            }
            // The check can load a class (and so collect): keep `this` and
            // read the two fields again afterwards.
            let this_pin = ctx.pin_native_root(this);
            let icce = mh_wmte_message_icce(ctx, &ids);
            this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            if let Some(icce) = icce {
                return Err(icce);
            }
            (rtype, ptypes) = read_types(&*ctx, this);
        }

        // `int` / `java.lang.String` -> `int` / `String`, which is what
        // `Class.getSimpleName()` yields and what the JDK's own `toString`
        // uses. Nested classes render after the last `$`, matching it.
        fn simple_name(
            ctx: &dyn cratonvm_native_api::NativeContext,
            mirror: Value,
        ) -> Result<String, MethodCallFailed> {
            let Value::Object(Some(m)) = mirror else {
                return Ok("?".to_string());
            };
            match resolve_class_name_robust(ctx, m) {
                Some(name) => {
                    // `Class.getSimpleName()` of the loaded class (wave 39):
                    // a local `Outer$1Local` is `Local`, an anonymous class
                    // `""`, a top-level `A$B` keeps its `$`.
                    let internal = name.replace('.', "/");
                    let dims = internal.bytes().take_while(|b| *b == b'[').count();
                    let loaded = if dims == 0 {
                        ctx.class_id_from_mirror(m)
                            .and_then(|id| simple_name_of_class(ctx, id, &internal))
                    } else {
                        internal[dims..]
                            .strip_prefix('L')
                            .and_then(|s| s.strip_suffix(';'))
                            .and_then(|elem| loaded_simple_name(ctx, elem))
                            .map(|s| s + &"[]".repeat(dims))
                    };
                    Ok(loaded.unwrap_or_else(|| class_display_simple_name(&name)))
                }
                // A lambda proxy's mirror names no class: its `getSimpleName()`
                // is its `getName()` past the package, `/0x..` suffix and all
                // (interpreter round i2 wave 61, lane L4).
                None => Ok(match lk_lambda_proxy_of_mirror(ctx, m) {
                    Some((_, name)) => name.rsplit('.').next().unwrap_or(&name).to_string(),
                    None => "?".to_string(),
                }),
            }
        }

        let mut out = String::from("(");
        // Tracked as a flag, not inferred from the rendered string: `()void` is
        // a perfectly good `MethodType`, so an empty parameter list must not be
        // read as "the field could not be read".
        let mut ptypes_ok = false;
        let mut param_count = 0usize;
        if let Value::Object(Some(arr)) = ptypes {
            ptypes_ok = true;
            param_count = ctx.array_length(arr);
            for i in 0..param_count {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&simple_name(ctx, ctx.get_array_element(arr, i))?);
            }
        }
        out.push(')');
        let ret = simple_name(ctx, rtype)?;
        let s = if !ptypes_ok || ret == "?" {
            // A field could not be read — do not invent a signature.
            ctx.create_string(&format!("MethodType({param_count} params)"))
        } else {
            out.push_str(&ret);
            ctx.create_string(&out)
        };
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- MethodHandle (1-field: name=0 for debugging) ---
    let mh = "java/lang/invoke/MethodHandle";
    // `type()` is NOT registered here. It was, with a body that returned
    // `ctx.get_field(this, 0)` raw, and it never ran once:
    // `register_t4_method_handle_invoke` runs after this registrar on every
    // boot arm (`vm/src/vm/vm_init.rs` :2594 and :3298, and
    // `native-builtins/src/lib.rs`'s synthetic chain) and registers the same
    // triple with the C19 body that prefers the real-JDK `type` field and
    // falls back to the synthetic descriptor.
    //
    // MEASURED, `--dump-native-registry --explain-jdk-only --real-jdk`:
    // `java/lang/invoke/MethodHandle.type()Ljava/lang/invoke/MethodType;`
    // appears twice, `owns_slot=false` here and `owns_slot=true` there.
    // Deleting the loser is inert; leaving it in place is a landmine, because a
    // lane retiring the WINNER on a census row would promote this raw slot-0
    // read in its place.
    // `MethodHandle.toString()` is `standardString()`: `"MethodHandle" +
    // type()` (`MethodHandle(int)int`, measured on JDK 25), which this
    // answered as the bare word `MethodHandle` for every handle until wave 47
    // (lane L4, probe `L4W47MethodHandleToString`). A handle whose `type`
    // cannot be read back keeps the bare word.
    r.register(mh, "toString", "()Ljava/lang/String;", |ctx, args| {
        let text = match args.first() {
            Some(Value::Object(Some(this))) => mh_declared_descriptor(ctx, *this)
                .and_then(|desc| method_type_display_in(&*ctx, &desc))
                .map(|shown| format!("MethodHandle{shown}")),
            _ => None,
        }
        .unwrap_or_else(|| "MethodHandle".to_string());
        let s = ctx.create_string_uninterned(&text);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- MemberName accessor overrides ---
    // The JDK bytecode for `MemberName.getMethodType()` reads its `type:Object`
    // field and, when null, invokes `expandFromVM` (a JVM-internal native) to
    // populate it. We don't implement expandFromVM, so when downstream code
    // (revealDirect, LambdaMetafactory) reads getMethodType the JDK falls
    // through to the null-return branch even after we populate the slot —
    // the JDK Object-field read may be intercepted by an injected vtable that
    // re-reads from a hidden vmtarget slot. Bypassing the Java logic entirely
    // with a native that returns slot 2 directly avoids the issue and matches
    // what `MemberName.type` would yield once `expandFromVM` had populated it.
    let mn_cls = "java/lang/invoke/MemberName";
    r.register(
        mn_cls,
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(mn_type_slot_converted(ctx, this)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `getMethodOrFieldType` is what `InfoFromMemberName.getMethodType()` and
    // several other JDK paths actually call. The JDK's own body answers the
    // method type for a method or constructor (slot 2 already holds it) and,
    // for a FIELD, the ACCESSOR's type: `methodType(fieldType)` for a getter,
    // `methodType(void.class, fieldType)` for a setter. This native returned
    // slot 2 raw for a field too -- a `Class` from a method declared to return
    // a `MethodType` -- so `revealDirect(findGetter(C, "i", int.class))
    // .getMethodType()` printed `class java.lang.Integer` where HotSpot prints
    // `()int` (wave 25, `L4W25RevealDirect.java`). A
    // field whose type cannot be spelled exactly keeps the old answer.
    r.register(
        mn_cls,
        "getMethodOrFieldType",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ty = mn_type_slot_converted(ctx, this);
            if let Value::Object(Some(field_type)) = ty {
                let ref_kind = match ctx.get_field(this, 3) {
                    Value::Int(flags) => (flags >> 24) & 0x0F,
                    _ => 0,
                };
                let is_class = ctx
                    .class_name_of_id(ctx.class_id_of_object(field_type))
                    .as_deref()
                    == Some("java/lang/Class");
                if (1..=4).contains(&ref_kind) && is_class {
                    if let Some(d) = mhn_strict_type_descriptor(&*ctx, field_type) {
                        // 1, 2: getField / getStatic; 3, 4: putField / putStatic.
                        let accessor = if ref_kind <= 2 {
                            format!("(){d}")
                        } else {
                            format!("({d})V")
                        };
                        if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &accessor) {
                            return Ok(Some(Value::Object(Some(mt))));
                        }
                    }
                }
            }
            Ok(Some(ty))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mn_cls,
        "getFieldType",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(mn_cls, "getModifiers", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Modifiers = low 16 bits of flags.
        let flags = match ctx.get_field(this, 3) {
            Value::Int(f) => f,
            _ => 0,
        };
        Ok(Some(Value::Int(flags & 0xFFFF)))    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(mn_cls, "getReferenceKind", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // flags is at declared-slot 3; refKind is the top byte
        let flags = match ctx.get_field(this, 3) {
            Value::Int(f) => f,
            _ => 0,
        };
        Ok(Some(Value::Int((flags >> 24) & 0x0F)))    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        mn_cls,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(mn_cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- MethodHandles (static utility) ---
    //
    // `lookup()` and `privateLookupIn(Class, Lookup)` are NOT registered here,
    // and the reason is the header comment thirty lines below this one, which
    // already stated the rule: *"we do not provide stub overrides here — the
    // real ones take precedence and a stub would only run if the late phase is
    // not also invoked"*. These two were violations of it, and they were the
    // dangerous kind.
    //
    // Both stubs allocated a bare `MethodHandles$Lookup` and returned it with
    // slot 0 — the lookup class — NEVER WRITTEN. That is verbatim defect #1 in
    // `native-builtins/tests/duplicate_registration_gate.rs`'s header: *"a
    // placeholder registered late shadowed the real implementation; the
    // returned Lookup's slot 0 was never written, so lookupClass() answered
    // null. Broke RJdkHidden AND RJdkStrict."*
    //
    // MEASURED, `--dump-native-registry --explain-jdk-only --real-jdk`: both
    // triples appear twice, `owns_slot=false` here and `owns_slot=true` in
    // `register_p63_method_handles_lookup`, which runs immediately after this
    // registrar on every boot arm — `vm/src/vm/vm_init.rs` :2588 and :3295,
    // and `phases_late.rs::register_phase63_natives` for the synthetic chain.
    // So the deletion is inert TODAY and removes the promotion hazard for
    // tomorrow.
    let mhs = "java/lang/invoke/MethodHandles";
    let _ = mhs;

    // NOT REGISTERED HERE: `MethodHandles.arrayElementVarHandle`.
    //
    // It is registered in `phases_late/reflect_invoke.rs`'s
    // `register_p59_varhandle`, which reaches the registry only through
    // `register_synthetic_overrides` — synthetic-JDK mode only. The obvious
    // move is to mirror it onto this (live in every mode) path so `--real-jdk`
    // gets a side-table entry for array handles. Measurement says do NOT:
    //
    //  * The real JDK bytecode for this factory ALREADY runs correctly in
    //    real-JDK mode and hands back a genuine `VarHandleInts$Array`.
    //    `vm/tests/clinit_first_call_compile_order.rs` is the witness — its
    //    probe does `arrayElementVarHandle(int[].class)`, `vh.set(arr,3,42)`,
    //    `vh.get(arr,3)` under `--java-home <real jdk>` and asserts `42`.
    //  * The accessors do not need the side table for the array case: every
    //    one of them routes through [`vh_array_call`], which recognises the
    //    access from the ARGUMENT SHAPE (`[vh, array, int]`) and never reads a
    //    synthetic slot off the receiver.
    //  * That same probe exists because `arrayElementVarHandle` is the cheapest
    //    trigger for the `VarForm` -> `MethodType` -> `sun/invoke/util/Wrapper`
    //    -> `ConstantDescs.<clinit>` chain. A native here short-circuits the
    //    chain and the regression test goes green while covering nothing.
    //
    // So the real object stays, and the two things it was missing are supplied
    // without replacing it: the bounds check lives on the `vh_array_call`
    // funnel, and `varType()`/`coordinateTypes()` read the real handle's own
    // identity in [`real_array_var_handle_descriptors`].

    // byteArrayViewVarHandle(<T>[].class, ByteOrder) — view a byte[] as a wider
    // primitive at a BYTE index. The real JDK VarHandle's get/set route through
    // our `varhandle_get`/`_set`, but those would mis-detect it as an
    // array-ELEMENT access (same [vh, array, index] arg shape) and read/write a
    // single byte — which silently zeroed SHA3/SHAKE XOF squeeze and thus
    // ML-DSA/ML-KEM keygen. Return a synthetic VarHandle tagged with byte-view
    // meta (element type from the array Class, endianness from the ByteOrder) so
    // get/set do the correct multi-byte LE/BE conversion. Also fixes any other
    // byteArrayViewVarHandle user (NIO-style serialization, crypto, …).
    r.register(
        mhs,
        "byteArrayViewVarHandle",
        "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let elem = match args.first() {
                Some(Value::Object(Some(mirror))) => {
                    let cid = ctx.class_id_from_mirror(*mirror);
                    let name = cid.and_then(|c| ctx.class_name_of_id(c));
                    // array class name is "[J" / "[I" / … → element descriptor is byte 1
                    name.and_then(|n| n.as_bytes().get(1).copied())
                        .unwrap_or(b'J')
                }
                _ => b'J',
            };
            let le = match args.get(1) {
                Some(Value::Object(Some(bo))) => match ctx.get_field_by_name(*bo, "name") {
                    Value::Object(Some(s)) => ctx
                        .read_string(s)
                        .map(|n| n.contains("LITTLE"))
                        .unwrap_or(true),
                    _ => true,
                },
                _ => true,
            };
            let vh =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
            vh_meta_put(
                ctx,
                vh,
                VarHandleMeta {
                    kind: if le {
                        VH_KIND_BYTE_VIEW_LE
                    } else {
                        VH_KIND_BYTE_VIEW_BE
                    },
                    class_name: String::new(),
                    field_name: String::new(),
                    field_desc: (elem as char).to_string(),
                    field_index: -1,
                    class_id: 0,
                    read_only: false,
                    exact: None,
                    ref_type_class_id: 0,
                    holder_class_id: 0,
                },
            );
            Ok(Some(Value::Object(Some(vh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // byteBufferViewVarHandle(<T>[].class, ByteOrder) -- view a ByteBuffer as
    // wider primitives at a BYTE index. Netty 4.2 uses these VarHandles for
    // direct ByteBuf multi-byte headers when USE_VAR_HANDLE is true.
    r.register(
        mhs,
        "byteBufferViewVarHandle",
        "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let elem = match args.first() {
                Some(Value::Object(Some(mirror))) => {
                    let cid = ctx.class_id_from_mirror(*mirror);
                    let name = cid.and_then(|c| ctx.class_name_of_id(c));
                    name.and_then(|n| n.as_bytes().get(1).copied())
                        .unwrap_or(b'J')
                }
                _ => b'J',
            };
            let le = match args.get(1) {
                Some(Value::Object(Some(bo))) => match ctx.get_field_by_name(*bo, "name") {
                    Value::Object(Some(s)) => ctx
                        .read_string(s)
                        .map(|n| n.contains("LITTLE"))
                        .unwrap_or(true),
                    _ => true,
                },
                _ => true,
            };
            let vh =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
            vh_meta_put(
                ctx,
                vh,
                VarHandleMeta {
                    kind: if le {
                        VH_KIND_BYTE_BUFFER_VIEW_LE
                    } else {
                        VH_KIND_BYTE_BUFFER_VIEW_BE
                    },
                    class_name: String::new(),
                    field_name: String::new(),
                    field_desc: (elem as char).to_string(),
                    field_index: -1,
                    class_id: 0,
                    read_only: false,
                    exact: None,
                    ref_type_class_id: 0,
                    holder_class_id: 0,
                },
            );
            Ok(Some(Value::Object(Some(vh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- MethodHandles.Lookup ---
    //
    // Note: findVirtual/findStatic/findGetter/findSetter/findConstructor and
    // friends are registered by the real implementations in
    // `register_p63_method_handles_lookup` (late phase). We do not provide
    // stub overrides here — the real ones take precedence and a stub would
    // only run if the late phase is not also invoked (which would indicate
    // a broken VM startup).
    //
    // `MethodHandles$Lookup.lookupClass()` is NOT registered here either — the
    // third of the same set, `owns_slot=false` in the dump, overwritten by
    // `register_p63_method_handles_lookup`. It is the READ side of the null
    // slot 0 the two deleted stubs above produced, so all three went together.

    // Lookup.defineHiddenClass: NOT registered here, deliberately.
    //
    // This file's registrar runs LATE (`register_phase54_method_handle`,
    // vm_init.rs:2648) and the triple table is last-write-wins, so the
    // placeholder that used to live here SHADOWED the real WP2.3-B
    // implementation in `lookup_define.rs:684` (wired on the essential path
    // at vm_init.rs:2120). The placeholder ignored its arguments and returned
    // a `MethodHandles$Lookup` whose slot 0 (`lookupClass`) was never written,
    // so `lookupClass()` answered null and every caller NPE'd on the result —
    // regression-suite RJdkHidden.defineNestmate:91 and RJdkStrict
    // .generatedClassesStillAllowed:247, in BOTH --fast-boot and --real-jdk.
    //
    // Note the header comment above this block already stated the intended
    // rule ("we do not provide stub overrides here — the real ones take
    // precedence"); this registration was the one violation of it.

    // --- VarHandle real ops ---
    let vh = "java/lang/invoke/VarHandle";

    // VarHandle.withInvokeExactBehavior() / withInvokeBehavior() — JDK builders
    // that toggle exact-invocation mode and return a VarHandle.
    // `sun.security.provider.SHA3.<clinit>` chains
    // `byteArrayViewVarHandle(...).withInvokeExactBehavior()`, which would
    // otherwise hit AbstractMethodError on our synthetic VarHandle. A handle
    // CratonVM minted gets a distinct copy with the other behaviour, as the
    // JDK's builders return (interpreter round i1 wave 37, lane L4;
    // `varhandle_with_exactness`) -- in `--compatible` too since JIT round 15
    // wave 1 and interpreter wave 49 (`CRATONVM_VH_EXACT_COMPATIBLE=0`
    // restores `this` there); a handle with no side-table row keeps the old
    // answer: `this`.
    //
    // DO NOT RETIRE — these two and `accessModeTypeUncached` below are
    // `declared, NO Code, not ACC_NATIVE` on all nine supported images
    // (MEASURED, `javap -p --system <image> java.lang.invoke.VarHandle`;
    // re-derivable with `scripts/jdk-only-no-image-methods.py`). The standard
    // retirement argument — "real JDK bytecode is behind it, so deleting the
    // native leaves something to run" — is FALSE for them: this registration is
    // the only implementation that exists for the receiver, and the sentence
    // above about `AbstractMethodError` is what the retirement would restore.
    // `H14-1` 4 sized that bucket at 2 rows because the corpus dispatched 2;
    // `H25-2` measured the registry and found 1,405 over 193 classes. These are
    // three of them, marked per `H25-2` N3.
    r.register(
        vh,
        "withInvokeExactBehavior",
        "()Ljava/lang/invoke/VarHandle;",
        |ctx, args| varhandle_with_exactness(ctx, args, true), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "withInvokeBehavior",
        "()Ljava/lang/invoke/VarHandle;",
        |ctx, args| varhandle_with_exactness(ctx, args, false), cratonvm_native_api::NativeKind::Bridge,
    );
    // Round 15 wave 1 (lane vh): `MethodHandles.collectReturnValue`, its kind
    // stated at its own row (see the block "`IndirectVarHandle`: round 15 wave 1").
    register_r15_vh_indirect_natives(r);

    r.register(
        vh,
        "accessModeTypeUncached",
        "(Ljava/lang/invoke/VarHandle$AccessType;)Ljava/lang/invoke/MethodType;",
        varhandle_access_mode_type_uncached, cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.varType() / coordinateTypes() — the describe-yourself pair.
    //
    // Both are CONCRETE bytecode in JDK 25, so they need the `check_override`
    // entry in `vm/src/vm/vm_exec.rs` (`class_name ==
    // "java/lang/invoke/VarHandle" && ("varType", "()Ljava/lang/Class;") |
    // ("coordinateTypes", "()Ljava/util/List;")`) to be consulted at all. The
    // real bodies cannot run here: both call `accessModeType`, which reads
    // `this.vform` and walks a `VarForm`/`MethodType` chain — and a CratonVM
    // VarHandle is `alloc_concurrent_synthetic("java/lang/invoke/VarHandle",
    // VH_FIELD_COUNT)` with CratonVM's own meaning imposed on the slots, so
    // slot 0 (`vform`) holds an `Int` kind tag. See RJdkHandles:244-245.
    //
    // REGISTERED HERE, not in `phases_late/reflect_invoke.rs`'s
    // `register_p59_varhandle`, and that is the whole point: `p59` reaches the
    // registry only through `register_phase59_natives` ->
    // `register_synthetic_overrides`, which `vm_init.rs` calls ONLY under
    // `config.use_synthetic_jdk`. In `--fast-boot`/`--real-jdk` the live
    // VarHandle surface is this file's (`register_phase54_method_handle` plus
    // `register_p63_method_handles_lookup`, both called from the real-JDK arm),
    // so a `varType` registered over there is dead in exactly the mode the
    // strict corpus runs. The answers come from the WP4.2 side table
    // (`VarHandleMeta`), which `findVarHandle`/`findStaticVarHandle` already
    // populate with the field descriptor and the declaring class name — no
    // layout change needed.
    r.register(vh, "varType", "()Ljava/lang/Class;", varhandle_var_type, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        vh,
        "coordinateTypes",
        "()Ljava/util/List;",
        varhandle_coordinate_types, cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.toMethodHandle(AccessMode) -> MethodHandle.
    //
    // Real JDK bytecode resolves this through `VarForm.memberName_table`, and
    // CratonVM's post-clinit fixup installs a VarForm stub whose four tables
    // are all null (`vm/src/vm/vm_util.rs`) -- so the real path produced a
    // MemberName with an EMPTY method name and the call died as
    // `NoSuchMethodError: java/lang/invoke/VarHandleReferences$FieldInstanceReadWrite.`
    // (note the missing method name). jboss-threads' `JDKSpecific$ThreadAccess`
    // (the JDK-24+ multi-release copy) does exactly this to null out
    // `Thread.threadLocals`, so EVERY worker-thread teardown reported
    // "terminated with error".
    //
    // Our VarHandles are side-table backed, so build the equivalent synthetic
    // getter/setter MethodHandle directly from that meta -- the same object
    // `Lookup.findGetter`/`findSetter` hand out, which this VM can dispatch.
    r.register(
        vh,
        "toMethodHandle",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodHandle;",
        varhandle_to_method_handle, cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.getMethodHandleUncached(int) -> MethodHandle: a reviewed
    // `Intrinsic` over concrete JDK bytecode (AGENTS.md), registered only
    // under `CRATONVM_VH_SYNTHETIC_MH_TABLE` (interpreter round i2 wave 57,
    // lane L4). A synthetic `VarHandle` has no `vform` for the JDK body to
    // read; every other receiver runs that body. Removal: when this VM's
    // `VarHandle`s carry a real `VarForm` (the proposal's design 1). See
    // `varhandle_get_method_handle_uncached`.
    if crate::nbflags().vh_synthetic_mh_table {
        r.register(
            vh,
            "getMethodHandleUncached",
            "(I)Ljava/lang/invoke/MethodHandle;",
            varhandle_get_method_handle_uncached,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    }

    // VarHandle.isAccessModeSupported(AccessMode) -> boolean.
    //
    // The JDK body asks `vform.getMemberNameOrNull(mode.ordinal())`, and a
    // CratonVM-minted handle has no real `vform`, so it was a
    // `NullPointerException` on every handle from `findVarHandle`,
    // `findStaticVarHandle`, `unreflectVarHandle` and the byte views
    // (interpreter round i1 wave 31). Answered from the side-table meta with
    // the rules the access natives enforce.
    r.register(
        vh,
        "isAccessModeSupported",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Z",
        varhandle_is_access_mode_supported,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.describeConstable() -> Optional<VarHandleDesc>. The JDK's
    // field and array handle classes override it; `VarHandle`'s own body,
    // the one a CratonVM-minted handle runs, answers `Optional.empty()`
    // (interpreter round i1 wave 45, lane L4, `L4W45VarHandleQueries`).
    r.register(
        vh,
        "describeConstable",
        "()Ljava/util/Optional;",
        varhandle_describe_constable,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.get(Object...) → Object
    // For instance fields: args = [receiver]; for static: args = []
    r.register(
        vh,
        "get",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_get,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.set(Object...) → void
    r.register(
        vh,
        "set",
        "([Ljava/lang/Object;)V",
        varhandle_set,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.compareAndSet(Object...) → boolean
    r.register(
        vh,
        "compareAndSet",
        "([Ljava/lang/Object;)Z",
        varhandle_compare_and_set,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.getAndSet(Object...) → Object
    r.register(
        vh,
        "getAndSet",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_get_and_set,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.getVolatile(Object...) → Object (same as get for now — no hardware fences)
    r.register(
        vh,
        "getVolatile",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_mode(ctx, args, "getVolatile"),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.setVolatile(Object...) → void. Its own callback, not
    // `varhandle_set`: a volatile store needs the StoreLoad fence the plain
    // `set`/`setOpaque`/`setRelease` do not (see `varhandle_set_volatile`).
    r.register(
        vh,
        "setVolatile",
        "([Ljava/lang/Object;)V",
        varhandle_set_volatile,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.getOpaque / getAcquire — read semantics
    r.register(
        vh,
        "getOpaque",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_mode(ctx, args, "getOpaque"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_mode(ctx, args, "getAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.setOpaque / setRelease — write semantics
    r.register(
        vh,
        "setOpaque",
        "([Ljava/lang/Object;)V",
        |ctx, args| varhandle_set_mode(ctx, args, "setOpaque"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "setRelease",
        "([Ljava/lang/Object;)V",
        |ctx, args| varhandle_set_mode(ctx, args, "setRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.compareAndExchange
    r.register(
        vh,
        "compareAndExchange",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_compare_and_exchange,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.getAndAdd and its ordering variants — single Object-return
    // registration per name. The polymorphic dispatcher in vm_exec.rs tries
    // the generic Object descriptor first and unbox_poly_return coerces the
    // result back to the call-site numeric type (H2 uses [III)I).
    r.register(
        vh,
        "getAndAdd",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_add_mode(ctx, args, "getAndAdd"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndAddAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_add_mode(ctx, args, "getAndAddAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndAddRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_add_mode(ctx, args, "getAndAddRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // VarHandle.getAndBitwise{Or,And,Xor} (+ ordering variants). java.net.Socket
    // updates its `state` int field via `STATE.getAndBitwiseOr(this, flag)`.
    r.register(
        vh,
        "getAndBitwiseOr",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOr"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseOrAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOrAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseOrRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOrRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseAnd",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAnd"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseAndAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAndAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseAndRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAndRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseXor",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXor"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseXorAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXorAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndBitwiseXorRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXorRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // C38: Ordering variants of compareAndSet / compareAndExchange / getAndSet.
    // The real JDK maps each to a distinct native; we use the same underlying
    // CAS / CAX / xchg implementation (we don't emit hardware fences). Needed
    // by Jackson's ConcurrentLinkedQueue which calls weakCompareAndSet.
    r.register(
        vh,
        "weakCompareAndSet",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSet"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "weakCompareAndSetPlain",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSetPlain"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "weakCompareAndSetAcquire",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSetAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "weakCompareAndSetRelease",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSetRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "compareAndExchangeAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_compare_and_exchange_mode(ctx, args, "compareAndExchangeAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "compareAndExchangeRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_compare_and_exchange_mode(ctx, args, "compareAndExchangeRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndSetAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_set_mode(ctx, args, "getAndSetAcquire"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        vh,
        "getAndSetRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_set_mode(ctx, args, "getAndSetRelease"),
        cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// VarHandle operation implementations
// ---------------------------------------------------------------------------

/// JDK-ONLY-LAYOUT: does this `VarHandle` object actually have OUR six-slot
/// layout, or is it a real `java.lang.invoke.VarHandle`?
///
/// `alloc_concurrent_synthetic` asks for `VH_FIELD_COUNT` slots, but when the
/// real class is loaded the object it returns has the **real** layout —
/// `vform` (a `VarForm` reference) at 0 and `exact` (a `boolean`) at 1 on
/// JDK 21+. Writing our `VH_KIND` `Int` to slot 0 is then coerced to `null`,
/// **destroying `vform`**, and writing our `VH_CLASS` `String` to slot 1 is
/// coerced to a number, corrupting `exact`.
///
/// Measured 2026-08-04 with `CRATONVM_DBG_OVERLAY=1`: 52 hits on each of those
/// two slots from `MhUtil.findVarHandle`, reached from the `<clinit>` of
/// `java.util.concurrent.atomic.AtomicBoolean`, `AtomicReference` and
/// `java.io.ObjectInputFilter$Config`. Those are real JDK classes whose
/// `static final VarHandle` fields real bytecode uses — so a null `vform` is
/// handed straight back to the JDK. It does not fault today only because our
/// natives intercept every `VarHandle` operation and read the side table
/// instead; the moment §7 step 3 hands one of those calls to real bytecode,
/// `vform.getMethodHandle(...)` is an NPE.
///
/// The metadata is not lost by skipping the writes: `vh_meta_put` is called on
/// every allocation path and every reader consults it first (see
/// [`vh_field_desc`]). The slot writes are the fallback for VarHandles
/// allocated outside our path, which by definition do not have our layout
/// either.
/// **Ask by NAME, not by field count.** The first version of this predicate was
/// `object_num_fields(vh) >= VH_FIELD_COUNT` and was completely inert: an A/B
/// against the pre-fix binary counted the same 8 overlay writes with and
/// without it. `alloc_concurrent_synthetic` returns an object with at least the
/// requested slot count either way, so a count test cannot tell the two layouts
/// apart — it only looks as though it can.
///
/// A real `java.lang.invoke.VarHandle` declares an instance field literally
/// named `vform`; a VM-fabricated stub has generated placeholder fields and
/// does not. That is the difference, so that is what is tested.
///
/// If a future JDK renames `vform`, this reverts to today's behaviour (writing
/// the slots) rather than to something new, and `CRATONVM_DBG_OVERLAY=1` still
/// reports it — a loud failure mode, not a silent one.
pub fn vh_has_synthetic_layout(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(vh);
    !mhn_declared_fields_named(&*ctx, class_id, "vform")
        .0
        .iter()
        .any(|f| !f.is_static && f.name == "vform")
}

/// Store the three name strings of a synthetic-layout VarHandle
/// (`VH_CLASS`, `VH_FIELD`, `VH_FIELD_DESC`) and hand back `vh`'s
/// post-allocation address.
///
/// gc-common w19-c (stale-handle triage; the `common-w18b` page's second
/// instance): `alloc_instance_var_handle` / `alloc_static_var_handle` held `vh`
/// raw across all three `create_string`s. Native allocation does not collect
/// today, so this was latent; the handle is rooted here so it stays correct
/// when an allocation can.
pub fn vh_store_names(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
) -> ObjectRef {
    let mut scope = NativeHandleScope::new(ctx);
    let vh_h = scope.root(vh);
    for (slot, text) in [
        (VH_CLASS, class_name),
        (VH_FIELD, field_name),
        (VH_FIELD_DESC, field_desc),
    ] {
        let s = scope.create_string(text);
        let vh_now = scope.get(&vh_h);
        scope.set_field(vh_now, slot, Value::Object(Some(s)));
    }
    scope.get(&vh_h)
}

/// Allocate a VarHandle for an instance field.
/// `field_index` is `i32`, not `usize`, because `-1` is a MEANINGFUL value here:
/// it is the "not resolved yet" sentinel that `varhandle_get`/`varhandle_set`
/// branch on to re-resolve the field BY NAME (see their `field_idx >= 0` test).
/// Both call sites used to launder a failed `resolve_field_index` through
/// `.unwrap_or(0)`, which is not "unresolved" — it is slot 0, a real field — so
/// the by-name fallback could never run and every access silently hit the
/// receiver's first field instead.
///
/// # JVMS 6.5 census: `VarHandle` is abstract too
///
/// Everything the `MethodHandle` block on [`alloc_method_handle`] records
/// applies here, with the same verdict (leave it, keep the WARN) and one extra
/// hazard of its own. AUDITED 2026-09-01.
///
/// `java.lang.invoke.VarHandle` is `ACC_ABSTRACT`; on a stock `cratonvm Hello`
/// the census names `alloc_static_var_handle` below simply because a static
/// field handle is the first of this file's four `VarHandle` mints that boot
/// reaches. The objects are real receivers -- returned from
/// `Lookup.findVarHandle` / `findStaticVarHandle`, held in `static final
/// VarHandle` fields of real JDK classes (`AtomicBoolean`, `AtomicReference`,
/// `ObjectInputFilter$Config`), and used as the receiver of every
/// signature-polymorphic `get` / `set` / `compareAndSet` / `getAndAdd` variant.
///
/// **(a) a concrete JDK subclass is worse here than for `MethodHandle`.** The
/// candidates (`IndirectVarHandle`, the generated `VarHandle*s$Field*` species)
/// all declare their own instance fields, and [`vh_has_synthetic_layout`] asks
/// `declared_fields(class_id)` for a field literally named `vform` -- a name
/// that lives on `VarHandle` itself, not on a subclass. A subclass receiver
/// would therefore be judged to have OUR layout and the six slot writes below
/// would run over the subclass's real fields. That predicate is in this file
/// and could be widened to walk the superclass chain, but the rest of the
/// price -- `vm_exec.rs`'s
/// `is_var_handle_signature_polymorphic_receiver`, a `typecheck.rs` cast arm,
/// and initialising the JDK's `java.lang.invoke` machinery on first use -- is
/// not.
///
/// **(b)** a `cratonvm/internal/...` stand-in loses `instanceof VarHandle` and
/// `checkcast VarHandle`, which currently answer correctly precisely because
/// the class is the abstract type. Same trade as the `MethodHandle` site, same
/// refusal.
///
/// **(c)** and the WARN cannot be silenced from this file even if both were
/// done: `java/lang/invoke/VarHandle` is minted at nine further sites in
/// `native-builtins/src/phases_late/foreign_ffm.rs` and
/// `native-builtins/src/phases_late/reflect_invoke.rs`, and the per-class dedupe
/// would just re-point `requester=` at one of them.
///
/// `class_name` / `class_id` are the handle's coordinate; `holder` is the
/// class the field was resolved in ([`VarHandleMeta::holder_class_id`]).
///
/// nio21 wave 11 L1 re-read this audit against HEAD and left the mint as it
/// is: the Spring Boot sample's `--compatible` census still names it, and the
/// way out (a `JdkInvoke` alignment row, so the JDK builds the handle as under
/// `--real-jdk`) is recorded on
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub fn alloc_instance_var_handle(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
    field_index: i32,
    class_id: cratonvm_types::ClassId,
    holder: Option<cratonvm_types::ClassId>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut vh: ObjectRef =
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
    // Only on OUR layout — see `vh_has_synthetic_layout`. On a real
    // `VarHandle` these six writes null `vform` and corrupt `exact`.
    if vh_has_synthetic_layout(ctx, vh) {
        ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_INSTANCE));
        vh = vh_store_names(ctx, vh, class_name, field_name, field_desc);
        ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(field_index));
        ctx.set_field(vh, VH_CLASS_ID, Value::Int(class_id.as_u32() as i32));
    }
    // Resolved once, here, for the JIT's thin write/CAS helpers — see
    // `VarHandleMeta::ref_type_class_id`. Both handle-minting callers pass a
    // `Class` mirror of the field type, so the type is already loaded; the
    // lookup only names it and never loads anything.
    // For a one-dimensional reference-array field it is the COMPONENT's id
    // instead (rt proposal W8-2); the plan tells the two apart by descriptor.
    let ref_type_class_id = vh_ref_type_lookup_name(field_desc)
        .or_else(|| vh_ref_array_component_lookup_name(field_desc))
        .and_then(|name| ctx.class_id_by_name(name))
        .map_or(0, |cid| cid.as_u32());
    // WP4.2: also stash in the side table so the descriptor-aware setter
    // on real-JDK VarHandle layout doesn't drop our metadata.
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: class_name.to_string(),
            field_name: field_name.to_string(),
            field_desc: field_desc.to_string(),
            field_index,
            class_id: class_id.as_u32(),
            read_only: false,
            exact: None,
            ref_type_class_id,
            holder_class_id: holder.map_or(0, |cid| cid.as_u32()),
        },
    );
    Ok(vh_install_real_varform(ctx, vh))
}

/// Allocate a VarHandle for a static field.
///
/// The JVMS 6.5 audit of this abstract-class mint is on
/// [`alloc_instance_var_handle`]; this is the site the boot WARN happens to
/// name, and the verdict there covers it unchanged.
pub fn alloc_static_var_handle(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
    owner: Option<ClassId>,
) -> Result<ObjectRef, MethodCallFailed> {
    let owner_raw = owner.map_or(0, |cid| cid.as_u32());
    let mut vh: ObjectRef =
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
    // Only on OUR layout — see `vh_has_synthetic_layout`.
    if vh_has_synthetic_layout(ctx, vh) {
        ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_STATIC));
        vh = vh_store_names(ctx, vh, class_name, field_name, field_desc);
        ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(-1)); // resolved lazily
        ctx.set_field(vh, VH_CLASS_ID, Value::Int(owner_raw as i32));
    }
    // WP4.2: side table for descriptor-aware-coercion-safe metadata access.
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_STATIC,
            class_name: class_name.to_string(),
            field_name: field_name.to_string(),
            field_desc: field_desc.to_string(),
            field_index: -1,
            class_id: owner_raw,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    Ok(vh_install_real_varform(ctx, vh))
}

// ---------------------------------------------------------------------------
// i57-L4 stages 1 and 2: a real `VarForm` for a minted field handle
// ---------------------------------------------------------------------------
//
// Interpreter round i6 wave 123, lane L4 (`CRATONVM_VH_REAL_VARFORM`, default
// off; `--compatible` only; proposal
// `docs/feature-designs/native-dispatch-architecture.md`).
//
// A field `VarHandle` this VM mints is the abstract `java/lang/invoke/VarHandle`
// with `vform == null`, so every JDK body that reads `vform` needs a native in
// front of it. Under the switch, a PLAIN (not exact) instance or static field
// handle gets a real `VarForm` built by the JDK's own constructor,
// `VarForm(implClass, receiver, value, intermediates...)`, exactly as
// `VarHandle<T>s.FieldInstanceReadOnly.FORM` is on HotSpot. Its `implClass` is
// one of the embedded classes below (stage 1): static methods named after the
// access modes, of the form's erased type with the `VarHandle` first, each one
// signature-polymorphic call on the handle (the natives serve it). A mode with
// no method is unsupported: `VarForm.resolveMemberName` answers `null`, which is
// what makes the JDK's `isAccessModeSupported` and `getMemberName`'s
// `UnsupportedOperationException` the JDK's own. One form per (class, shape).
//
// Interpreter round i7 wave 136, lane L4 (page
// `i135-L4-real-varform-classes-are-application-visible`): the classes are
// defined HIDDEN (JEP 371), so `Class.forName` never finds them and a trace
// never names a `cratonvm.internal` frame (a hidden class's frames are hidden).
// HotSpot lists its form method's frame there
// (`java.lang.invoke.VarHandleInts$FieldInstanceReadOnly.get`); this VM now
// lists nothing in its place, a missing line instead of a wrong one (the
// residual is recorded on the page). The read-write class links to its read-only
// superclass by id (`DefineClassFull::superclass_id_override`). Their ids and
// the shared forms (as global roots) live in a per-VM record
// ([`VhRealVarFormRecord`], `NativeContext::vh_real_varform_slot`), not in
// the classes' `FORM_INSTANCE` / `FORM_STATIC` statics, which stay `null`.
//
// The mint is reached only where this VM mints a field handle: under
// `--compatible` with the `jdk-invoke` alignment row OFF
// (`CRATONVM_COMPAT_ALIGN=-jdk-invoke`), since that row (default ON) leaves
// `Lookup.findVarHandle` / `findStaticVarHandle` / `unreflectVarHandle` to the
// JDK, whose handles carry the JDK's own forms.
//
// Not given a form, and so served as before: an exact copy
// (`withInvokeExactBehavior`; the helpers' erased call sites would be refused
// by its exactness check), array element and byte-view handles, and every
// handle under `--real-jdk` (the JDK's factories run there).
//
// Sources and the generator: `native-builtins/src/class_bytes/vhform/`.

/// One variable type's pair of embedded `implClass`es: the read-only class
/// (the read modes) and the read-write class that extends it.
pub struct VhFormClass {
    pub letter: u8,
    pub primitive: Option<&'static str>,
    pub ro_name: &'static str,
    pub ro_bytes: &'static [u8],
    pub rw_name: &'static str,
    pub rw_bytes: &'static [u8],
}

pub static VH_REAL_VARFORM_CLASSES: [VhFormClass; 9] = [
    VhFormClass {
        letter: b'Z',
        primitive: Some("boolean"),
        ro_name: "cratonvm/internal/VhFormBooleanReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormBooleanReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormBoolean",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormBoolean.class"),
    },
    VhFormClass {
        letter: b'B',
        primitive: Some("byte"),
        ro_name: "cratonvm/internal/VhFormByteReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormByteReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormByte",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormByte.class"),
    },
    VhFormClass {
        letter: b'C',
        primitive: Some("char"),
        ro_name: "cratonvm/internal/VhFormCharReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormCharReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormChar",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormChar.class"),
    },
    VhFormClass {
        letter: b'S',
        primitive: Some("short"),
        ro_name: "cratonvm/internal/VhFormShortReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormShortReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormShort",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormShort.class"),
    },
    VhFormClass {
        letter: b'I',
        primitive: Some("int"),
        ro_name: "cratonvm/internal/VhFormIntReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormIntReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormInt",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormInt.class"),
    },
    VhFormClass {
        letter: b'J',
        primitive: Some("long"),
        ro_name: "cratonvm/internal/VhFormLongReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormLongReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormLong",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormLong.class"),
    },
    VhFormClass {
        letter: b'F',
        primitive: Some("float"),
        ro_name: "cratonvm/internal/VhFormFloatReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormFloatReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormFloat",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormFloat.class"),
    },
    VhFormClass {
        letter: b'D',
        primitive: Some("double"),
        ro_name: "cratonvm/internal/VhFormDoubleReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormDoubleReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormDouble",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormDouble.class"),
    },
    VhFormClass {
        letter: b'L',
        primitive: None,
        ro_name: "cratonvm/internal/VhFormReferenceReadOnly",
        ro_bytes: include_bytes!("../../class_bytes/vhform/VhFormReferenceReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormReference",
        rw_bytes: include_bytes!("../../class_bytes/vhform/VhFormReference.class"),
    },
];

/// The prefix every `VH_REAL_VARFORM_CLASSES` name shares. Since wave 136
/// [`vh_has_real_varform_of_ours`] compares ids with the per-VM record instead.
pub const VH_REAL_VARFORM_PREFIX: &str = "cratonvm/internal/VhForm";

/// The embedded classes for a field descriptor's variable type (`L` and `[`
/// share the reference class: the form's types are erased).
pub fn vh_real_varform_class(field_desc: &str) -> Option<&'static VhFormClass> {
    let letter = match field_desc.as_bytes().first()? {
        b'L' | b'[' => b'L',
        other => *other,
    };
    VH_REAL_VARFORM_CLASSES.iter().find(|c| c.letter == letter)
}

/// The per-VM record of `CRATONVM_VH_REAL_VARFORM` (interpreter round i7 wave
/// 136, lane L4; page `i135-L4-real-varform-classes-are-application-visible`),
/// in `NativeContext::vh_real_varform_slot`: the hidden form classes this VM
/// defined and its shared forms. Atomics only (no lock is held across a
/// `NativeContext` call).
#[derive(Default)]
pub struct VhRealVarFormRecord {
    /// Per `VH_REAL_VARFORM_CLASSES` row, `[read-only, read-write]`: the
    /// hidden class's raw `ClassId` plus one (`0`: not defined yet).
    pub ids: [[std::sync::atomic::AtomicU32; 2]; 9],
    /// Per row, `[read-only, read-write]`, `[static, instance]`: the global
    /// root (`NativeContext::add_global_root`) of the shared form (`0`: none
    /// yet). A global root lives as long as the VM and moves with the form.
    pub forms: [[[std::sync::atomic::AtomicUsize; 2]; 2]; 9],
    /// A define of form classes is in flight: another minter declines (its
    /// handle is served as before) rather than wait, so no thread blocks here
    /// and a re-entry cannot deadlock.
    pub defining: std::sync::atomic::AtomicBool,
}

/// THIS VM's record, created at the first call; `None` for a context with no
/// per-VM slot (the mocks: no form is built).
pub fn vh_real_varform_record(ctx: &dyn NativeContext) -> Option<&VhRealVarFormRecord> {
    let slot = ctx.vh_real_varform_slot()?;
    let boxed = slot.get_or_init(|| -> Box<dyn std::any::Any + Send + Sync> {
        Box::new(VhRealVarFormRecord::default())
    });
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    any.downcast_ref::<VhRealVarFormRecord>()
}

/// `class`'s row in `VH_REAL_VARFORM_CLASSES` (and in the record).
pub fn vh_real_varform_row(class: &VhFormClass) -> Option<usize> {
    VH_REAL_VARFORM_CLASSES
        .iter()
        .position(|c| c.letter == class.letter)
}

/// The recorded id of row `row`'s read-only (`rw == false`) or read-write class.
pub fn vh_real_varform_recorded_id(ctx: &dyn NativeContext, row: usize, rw: bool) -> Option<ClassId> {
    let raw = vh_real_varform_record(ctx)?.ids.get(row)?[usize::from(rw)]
        .load(std::sync::atomic::Ordering::Acquire);
    raw.checked_sub(1).map(ClassId::new)
}

/// Define one embedded class HIDDEN, under its own name (unique in this VM
/// unless something else took the name: then the class manager mangles it and
/// a read-write define over it is refused, fail-closed), linked to
/// `superclass` by id when given. Runs no Java (a hidden define is offered to
/// no transformer); a GC point.
pub fn vh_real_varform_define_hidden(
    ctx: &mut dyn NativeContext,
    name: &str,
    bytes: &[u8],
    superclass: Option<ClassId>,
) -> Option<ClassId> {
    ctx.define_class_full(
        name,
        bytes,
        0,
        cratonvm_native_api::DefineClassFull {
            override_name: Some(name.to_string()),
            hidden: true,
            superclass_id_override: superclass,
            ..Default::default()
        },
    )
    .ok()
}

/// The hidden class of row `row` (`rw`: the read-write one), defined once per
/// VM: the read-only class first, since it is the read-write class's
/// superclass. `None` when a define fails or another thread is defining (the
/// handle then stays as it is).
pub fn vh_real_varform_define(
    ctx: &mut dyn NativeContext,
    row: usize,
    class: &VhFormClass,
    rw: bool,
) -> Option<ClassId> {
    use std::sync::atomic::Ordering;
    if let Some(cid) = vh_real_varform_recorded_id(&*ctx, row, rw) {
        return Some(cid);
    }
    vh_real_varform_record(&*ctx)?
        .defining
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let defined = vh_real_varform_define_held(ctx, row, class, rw);
    if let Some(record) = vh_real_varform_record(&*ctx) {
        record.defining.store(false, Ordering::Release);
    }
    defined
}

/// [`vh_real_varform_define`]'s defines, run while it holds the record's
/// `defining` word: each class not recorded yet is defined and recorded.
pub fn vh_real_varform_define_held(
    ctx: &mut dyn NativeContext,
    row: usize,
    class: &VhFormClass,
    rw: bool,
) -> Option<ClassId> {
    use std::sync::atomic::Ordering;
    let ro = match vh_real_varform_recorded_id(&*ctx, row, false) {
        Some(ro) => ro,
        None => {
            let ro = vh_real_varform_define_hidden(ctx, class.ro_name, class.ro_bytes, None)?;
            vh_real_varform_record(&*ctx)?.ids[row][0]
                .store(ro.as_u32().checked_add(1)?, Ordering::Release);
            ro
        }
    };
    if !rw {
        return Some(ro);
    }
    match vh_real_varform_recorded_id(&*ctx, row, true) {
        Some(cid) => Some(cid),
        None => {
            let cid = vh_real_varform_define_hidden(ctx, class.rw_name, class.rw_bytes, Some(ro))?;
            vh_real_varform_record(&*ctx)?.ids[row][1]
                .store(cid.as_u32().checked_add(1)?, Ordering::Release);
            Some(cid)
        }
    }
}

/// The recorded shared form of row `row`, `rw`, `instance`, where it is now.
pub fn vh_real_varform_recorded_form(
    ctx: &dyn NativeContext,
    row: usize,
    rw: bool,
    instance: bool,
) -> Option<ObjectRef> {
    let handle = vh_real_varform_record(ctx)?.forms.get(row)?[usize::from(rw)]
        [usize::from(instance)]
    .load(std::sync::atomic::Ordering::Acquire);
    if handle == 0 {
        return None;
    }
    ctx.resolve_global_root(handle)
}

/// File `form` as the shared form of row `row`, `rw`, `instance`, unless
/// another thread filed one first (then that one is answered and `form` is
/// dropped: two valid forms, one kept). Allocates nothing; no GC point.
pub fn vh_real_varform_file_form(
    ctx: &mut dyn NativeContext,
    row: usize,
    rw: bool,
    instance: bool,
    form: ObjectRef,
) -> ObjectRef {
    use std::sync::atomic::Ordering;
    let handle = ctx.add_global_root(form);
    if handle == 0 {
        return form;
    }
    let filed = vh_real_varform_record(&*ctx).map(|record| {
        record.forms[row][usize::from(rw)][usize::from(instance)].compare_exchange(
            0,
            handle,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
    });
    match filed {
        Some(Ok(_)) => form,
        Some(Err(winner)) => {
            ctx.remove_global_root(handle);
            ctx.resolve_global_root(winner).unwrap_or(form)
        }
        None => {
            ctx.remove_global_root(handle);
            form
        }
    }
}

/// The shared `VarForm` of `class` (read-only or read-write) for an instance
/// (`(Object)T` get type) or static (`()T`) field: the one in this VM's record,
/// or a new one built by the JDK's constructor and recorded. `None` when
/// anything cannot be defined or built (the handle then stays as it is).
/// Allocates and runs Java: the caller pins what it holds.
pub fn vh_real_varform_shared(
    ctx: &mut dyn NativeContext,
    class: &VhFormClass,
    read_only: bool,
    instance: bool,
) -> Option<ObjectRef> {
    let row = vh_real_varform_row(class)?;
    let rw = !read_only;
    if let Some(form) = vh_real_varform_recorded_form(&*ctx, row, rw, instance) {
        return Some(form);
    }
    let cid = vh_real_varform_define(ctx, row, class, rw)?;
    // No `<clinit>` (the statics have no initializer): this only marks the
    // class initialized, so the first call through a form meets no
    // initialization barrier. (Its `FORM_INSTANCE` / `FORM_STATIC` statics are
    // no longer written: the forms live in the record.)
    ctx.ensure_class_initialized_with_class_id(cid).ok()?;
    let object_id = ctx.class_id_by_name("java/lang/Object")?;
    let class_class_id = ctx.class_id_by_name("java/lang/Class")?;
    // Every mirror and the empty `Class[]` are pinned across the allocations
    // that follow them and the constructor call.
    let impl_mirror = ctx.get_class_mirror(cid);
    let base = ctx.pin_native_root(impl_mirror);
    let object_mirror = ctx.get_class_mirror(object_id);
    let object_pin = ctx.pin_native_root(object_mirror);
    let value_mirror = match class.primitive {
        Some(name) => ctx.primitive_class_mirror(name),
        None => ctx.read_native_pin(object_pin, object_mirror),
    };
    let value_pin = ctx.pin_native_root(value_mirror);
    let none = ctx.new_ref_array(class_class_id, 0);
    let none_pin = ctx.pin_native_root(none);
    let impl_mirror = ctx.read_native_pin(base, impl_mirror);
    let object_mirror = ctx.read_native_pin(object_pin, object_mirror);
    let value_mirror = ctx.read_native_pin(value_pin, value_mirror);
    let none = ctx.read_native_pin(none_pin, none);
    // `VarForm(implClass, receiver, value, intermediate...)`: a `null`
    // receiver is a static field's form (no coordinate), as the JDK's
    // `FieldStaticReadOnly.FORM` is built.
    let receiver = if instance {
        Value::Object(Some(object_mirror))
    } else {
        Value::Object(None)
    };
    let built = ctx.new_object_initialized(
        "java/lang/invoke/VarForm",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)V",
        &[
            Value::Object(Some(impl_mirror)),
            receiver,
            Value::Object(Some(value_mirror)),
            Value::Object(Some(none)),
        ],
    );
    ctx.unpin_native_roots(base);
    let form = match built {
        Ok(Some(Value::Object(Some(form)))) => form,
        _ => return None,
    };
    Some(vh_real_varform_file_form(ctx, row, rw, instance, form))}

/// Give `vh`, a handle this VM just minted (or just marked read-only), the
/// real `VarForm` of its row under `CRATONVM_VH_REAL_VARFORM` (see the block
/// comment above). Answers `vh`'s current address (building a form is a GC
/// point). A no-op when the switch is off, under `--real-jdk`, for a
/// synthetic-layout handle, an exact one, or any kind but an instance or static
/// field; and when the form cannot be built (the handle keeps `vform == null`
/// and the natives in front serve it, as before).
pub fn vh_install_real_varform(ctx: &mut dyn NativeContext, vh: ObjectRef) -> ObjectRef {
    if !crate::nbflags().vh_real_varform || ctx.is_jdk_only() || vh_has_synthetic_layout(ctx, vh)
    {
        return vh;
    }
    let Some(meta) = vh_meta_get(ctx, vh) else {
        return vh;
    };
    if meta.exact.is_some() || !(meta.kind == VH_KIND_INSTANCE || meta.kind == VH_KIND_STATIC) {
        return vh;
    }
    let Some(class) = vh_real_varform_class(&meta.field_desc) else {
        return vh;
    };
    let (read_only, instance) = (meta.read_only, meta.kind == VH_KIND_INSTANCE);
    drop(meta);
    let pin = ctx.pin_native_root(vh);
    let form = vh_real_varform_shared(ctx, class, read_only, instance);
    let vh = ctx.read_native_pin(pin, vh);
    ctx.unpin_native_roots(pin);
    let Some(form) = form else {
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!("[VH_REAL_VARFORM] no form for a {} handle: served as before", class.rw_name);        }
        return vh;
    };
    let class_id = ctx.class_id_of_object(vh);
    if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "vform") {
        ctx.set_field(vh, slot, Value::Object(Some(form)));
        if crate::nbflags().dbg_mh_dispatch {
            let name = if read_only {
                class.ro_name
            } else {
                class.rw_name
            };
            let shape = if instance { "instance" } else { "static" };
            eprintln!("[VH_REAL_VARFORM] {shape} field handle: VarForm over {name}");
        }
    }
    vh
}

/// `vh` carries a `VarForm` [`vh_install_real_varform`] gave it (its
/// `implClass` is one of `VH_REAL_VARFORM_CLASSES`), so the JDK's own bodies
/// of `isAccessModeSupported` / `toMethodHandle` answer it. `false` whenever
/// `CRATONVM_VH_REAL_VARFORM` is off. Reads only.
pub fn vh_has_real_varform_of_ours(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    if !crate::nbflags().vh_real_varform {
        return false;
    }
    let Value::Object(Some(form)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return false;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, form, "java/lang/invoke/VarForm", "implClass")
    else {
        return false;
    };
    // Wave 136 (page `i135-L4-real-varform-classes-are-application-visible`):
    // by id against this VM's record, not by name prefix, so a class an
    // application defined under a `cratonvm/internal/VhForm` name is never
    // taken for one of ours.
    let Some(raw) = ctx
        .class_id_from_mirror(impl_mirror)
        .and_then(|cid| cid.as_u32().checked_add(1))
    else {
        return false;
    };
    vh_real_varform_record(&*ctx).is_some_and(|record| {
        record
            .ids
            .iter()
            .flatten()
            .any(|id| id.load(std::sync::atomic::Ordering::Acquire) == raw)
    })
}


/// Resolve a STATIC VarHandle's storage slot: `(class_id, static-block index)`.
///
/// The value of a static field lives in the class's static-field storage, NOT
/// in the `Class` mirror object's instance slots. The earlier STATIC branches
/// used `get_field_by_name(mirror, field)` / `set_field_by_name(mirror, field)`,
/// which read/write the *mirror object* — so the lookup found no such field and
/// silently returned the default (0) / dropped the write. Callers must instead
/// go through `get_static_field` / `set_static_field` with the index this
/// resolves. Returns `Ok(None)` if the class isn't loaded or has no such
/// static field, and the `<clinit>` failure (`ExceptionInInitializerError`,
/// then `NoClassDefFoundError`) as `Err`, as `getstatic` / `putstatic` would
/// throw it; the handle used to answer a default and drop the write (wave 17).
///
/// `live` holds the caller's operand values (the value to store, the CAS
/// witness): the holder's `<clinit>` is a GC point, so they are pinned across
/// it and refreshed in place (only when the holder is not yet initialized).
///
/// `init` names the access (wave 59, lane L4): a recorded holder's
/// initialization failure lists `LazyInitializingVarHandle`'s frames
/// ([`vh_static_init_chain`]).
pub fn vh_static_slot(
    ctx: &mut dyn NativeContext,
    class: &str,
    field: &str,
    owner: Option<ClassId>,
    live: &mut [Value],
    init: VhStaticInit,
) -> Result<Option<(ClassId, usize)>, MethodCallFailed> {
    // Mirror HotSpot: the first ACCESS through a static-field VarHandle triggers
    // the holder class's `<clinit>` (as `getstatic`/`putstatic` would). Without
    // this, a static field read via `findStaticVarHandle().get()` on a not-yet-
    // initialized class returns the field's default (`null`/0) instead of the
    // value its `<clinit>` assigns — e.g. Caffeine's `LocalCacheFactory.newFactory`
    // reads `<generated>.FACTORY` (set in that class's `<clinit>`) and got `null`,
    // failing every Hibernate SessionFactory build. Idempotent + cheap once the
    // class is initialized; the recursive-init guard handles the in-`<clinit>`
    // case (the initializing thread may access the field during its own clinit).
    // Strict mode records the exact class the handle was minted against; two
    // loaders that define one name are two classes, and by-name resolution
    // reads and initialises the wrong one (see `mh_static_owner_of_mirror`).
    //
    // An INHERITED static (`findStaticVarHandle(C.class, "f", ..)` for an `f`
    // that `P` declares) lives in, and initializes, its declaring class: the
    // by-name arm below found no `f` in `C` and answered nothing (wave 16).
    let named = match owner {
        Some(cid) => Some(cid),
        None => ctx.class_id_by_name(class),
    };
    let inherited = named.and_then(|n| inherited_static_field_holder(&*ctx, n, field));
    if let Some(holder) = inherited {
        vh_initialize_static_holder(ctx, holder, live, init)?;
        return Ok(ctx
            .static_field_index_by_name(holder, field)
            .map(|idx| (holder, idx)));
    }
    let cid = match owner {
        Some(cid) => {
            vh_initialize_static_holder(ctx, cid, live, init)?;
            cid
        }
        None => {
            let settled = named.is_some_and(|c| ctx.is_class_initialized(c));
            let (base, handles) = if settled {
                (usize::MAX, Vec::new())
            } else {
                pin_mh_args(ctx, live)
            };
            // A holder not yet initialized lists `LazyInitializingVarHandle`'s
            // frames (wave 59, lane L4); a settled one costs what it did.
            let init = if !settled && vh_static_mode_method(init.mode, init.variable).is_some() {
                ctx.ensure_class_initialized_served(class, &|| {
                    vh_static_init_chain(init).unwrap_or_else(|| {
                        cratonvm_native_api::ServedRaiseChain {
                            throwable_class: "java/lang/ExceptionInInitializerError",
                            hops: Vec::new(),
                        }
                    })
                })
            } else {
                ctx.ensure_class_initialized(class)
            };
            vh_refresh_live(ctx, base, &handles, live);
            // A class that does not load has no slot (the old answer); a
            // class that loads and fails its `<clinit>` throws.
            let Some(cid) = ctx.class_id_by_name(class) else {
                return Ok(None);
            };
            init?;
            cid
        }
    };
    Ok(ctx
        .static_field_index_by_name(cid, field)
        .map(|idx| (cid, idx)))
}

/// Initialize a static VarHandle's holder, keeping `live` valid across the
/// `<clinit>` (see [`vh_static_slot`]). Pins only when the holder is not yet
/// initialized, so a steady-state access costs one state read.
pub fn vh_initialize_static_holder(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    live: &mut [Value],
    site: VhStaticInit,
) -> Result<(), MethodCallFailed> {
    if ctx.is_class_initialized(holder) {
        return ctx.ensure_class_initialized_with_class_id(holder);
    }
    vh_initialize_static_holder_cold(ctx, holder, live, site)
}

#[cold]
#[inline(never)]
pub fn vh_initialize_static_holder_cold(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    live: &mut [Value],
    site: VhStaticInit,
) -> Result<(), MethodCallFailed> {
    let (base, handles) = if live.is_empty() {
        (usize::MAX, Vec::new())
    } else {
        pin_mh_args(ctx, live)
    };
    let init = match vh_static_init_chain(site) {
        Some(chain) => ctx.initialize_class_served(holder, chain),
        None => ctx.ensure_class_initialized_with_class_id(holder),
    };
    vh_refresh_live(ctx, base, &handles, live);
    init
}

/// The access a static VarHandle's holder is initialized for (wave 59, lane
/// L4): the access mode's name and the variable's descriptor byte (`0`: not
/// known, no frames are listed).
#[derive(Clone, Copy, Debug)]
pub struct VhStaticInit {
    pub mode: &'static str,
    pub variable: u8,
}

/// [`VhStaticInit`] of the access `mode` through the handle whose row is
/// `meta`.
#[inline]
pub fn vh_static_init(mode: &'static str, meta: Option<&VarHandleMeta>) -> VhStaticInit {
    VhStaticInit {
        mode,
        variable: meta
            .and_then(|m| m.field_desc.as_bytes().first().copied())
            .unwrap_or(0),
    }
}

// ── j27 wave 1 (lane FFMVH; proposal
// `docs/feature-designs/native-dispatch-architecture.md`,
// page `jr26-static-varhandle-rmw-is-a-native-call-FIXED-20261029.md`): the static
// arms' `(holder, slot)` memoised per handle ──────────────────────────────────
//
// Every static `VarHandle` access through the natives cloned the row's class
// and field names and re-ran `vh_static_slot` -- the inherited-holder walk,
// the holder's initialization test and a by-name static-field lookup -- for
// an answer that cannot change once the holder is initialized: the handle's
// class and field are final, and a static's slot is fixed for the class's
// life (redefinition may not change a class's fields).
//
// A line is filled only after `vh_static_slot` answered for an INITIALIZED
// holder, so the `<clinit>` paths (and their `LazyInitializingVarHandle`
// frames, `ExceptionInInitializerError` / `NoClassDefFoundError`) always run
// through `vh_static_slot` exactly as before. The line is keyed as
// [`vh_memo_lookup`]'s lines are -- VM identity, identity hash, ADDRESS and
// [`VH_META_GENERATION`] (moved by every moving collection and every change
// to the handle tables) -- plus the VM's count of unloaded classes, so an
// unload anywhere discards every line (a `ClassId` is never served past the
// unload of its class). A hit is also re-checked against
// `is_class_initialized`, which a class reset in place (a stub upgraded,
// `ClassInitBits::note_init_reset`) clears. No line is kept for a context
// without a VM identity (`0`: mock contexts share it). Per thread, per VM: no
// process state, and nothing a dropped VM's line could pin (ids only).

/// `CRATONVM_VH_STATIC_SLOT_MEMO` (j27 wave 1, lane FFMVH; perf, default ON,
/// `=0` keeps every static `VarHandle` access on the full by-name resolution).
/// Read only when a line would be FILLED (once per handle, thread and
/// generation), never on a hit: with the switch off no line is ever filled,
/// so no hit can happen.
pub fn vh_static_slot_memo_enabled() -> bool {
    true
}

/// One memoised static slot (see above).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VhStaticSlotLine {
    pub class_id: ClassId,
    pub slot: usize,
    /// The VM's `unloaded_class_count` when the line was filled.
    pub unloaded: u64,
}

thread_local! {
    /// Per-thread memo of [`vh_static_slot_of`].
    pub static VH_STATIC_SLOT_MEMO: std::cell::RefCell<Vec<VhMemoLine<VhStaticSlotLine>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// The key of `vh`'s line, read BEFORE the resolution (which can run a
/// `<clinit>`, and so a collection that moves `vh`: the generation then moves
/// on and the line filled under this key is never served).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VhStaticSlotKey {
    pub vm: usize,
    pub hash: i32,
    pub addr: usize,
    pub generation: u64,
    pub unloaded: u64,
}

pub fn vh_static_slot_key(ctx: &dyn NativeContext, vh: ObjectRef) -> VhStaticSlotKey {
    VhStaticSlotKey {
        vm: ctx.vm_identity(),
        hash: ctx.identity_hash_code(vh),
        addr: vh.as_ptr() as usize,
        generation: VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire),
        unloaded: ctx.unloaded_class_count(),
    }
}

/// The memoised `(holder, slot)` under `key`, or `None`.
pub fn vh_static_slot_memo_get(
    ctx: &dyn NativeContext,
    key: VhStaticSlotKey,
) -> Option<(ClassId, usize)> {
    if key.vm == 0 {
        return None;
    }
    let index = (key.hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let line = VH_STATIC_SLOT_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[index];
        if line.vm == key.vm
            && line.key == key.hash
            && line.addr == key.addr
            && line.generation == key.generation
        {
            line.value.filter(|v| v.unloaded == key.unloaded)
        } else {
            None
        }
    })?;
    ctx.is_class_initialized(line.class_id)
        .then_some((line.class_id, line.slot))
}

/// File `(class_id, slot)` under `key` (a no-op without a VM identity).
pub fn vh_static_slot_memo_put(key: VhStaticSlotKey, class_id: ClassId, slot: usize) {
    if key.vm == 0 {
        return;
    }
    let index = (key.hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    VH_STATIC_SLOT_MEMO.with(|memo| {
        memo.borrow_mut()[index] = VhMemoLine {
            vm: key.vm,
            key: key.hash,
            addr: key.addr,
            generation: key.generation,
            value: Some(VhStaticSlotLine {
                class_id,
                slot,
                unloaded: key.unloaded,
            }),
        };
    });
}

/// [`vh_static_slot`] for the static `VarHandle` `this` (its row `meta`, or
/// its own `VH_CLASS` / `VH_FIELD` strings), through the per-handle memo
/// above. Same answers, same errors, same `live` contract: a hit touches
/// nothing (no `<clinit>` can run for an initialized holder).
pub fn vh_static_slot_of(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    live: &mut [Value],
    init: VhStaticInit,
) -> Result<Option<(ClassId, usize)>, MethodCallFailed> {
    let key = vh_static_slot_key(&*ctx, this);
    if let Some(hit) = vh_static_slot_memo_get(&*ctx, key) {
        return Ok(Some(hit));
    }
    let (class, field) = vh_static_class_field(ctx, this, meta);
    let owner = vh_static_owner(meta);
    let found = vh_static_slot(ctx, &class, &field, owner, live, init)?;
    if let Some((class_id, slot)) = found {
        if ctx.is_class_initialized(class_id) {
            if key.vm != 0 && vh_static_slot_memo_enabled() {
                vh_static_slot_memo_put(key, class_id, slot);
            }
            vh_static_note_target(&*ctx, this, meta, class_id, slot);
        }
    }
    Ok(found)
}

/// j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): tell the VM which
/// static the handle `this` names, once its holder is initialized -- the
/// filing of the compiled static constant-handle cells
/// (`vm/src/jit/vh_static_cells.rs`). Asked where the memo above fills a line
/// (once per handle, thread and generation), so a hit costs nothing. Only a
/// handle with a row may be written inline: its row says read-write and not
/// invoke-exact.
pub fn vh_static_note_target(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    class_id: ClassId,
    slot: usize,
) {
    let writable = meta.is_some_and(|m| !m.read_only && m.exact.is_none());
    let desc = || match meta.map(|m| m.field_desc.as_str()) {
        Some("I") => Some(b'I'),
        Some("J") => Some(b'J'),
        _ => None,
    };
    cratonvm_native_api::NativeSystemAccess::note_static_varhandle_target(
        ctx, this, class_id, slot, writable, &desc,
    );
}


/// `VarHandle<T>s$FieldStatic{ReadOnly,ReadWrite}` and the descriptor of its
/// static method for the access `mode` of a variable of descriptor byte
/// `variable` (JDK 25: the read modes are declared in `ReadOnly`, every other
/// mode in `ReadWrite`; a reference variable is the `References` family's
/// `Object`). `None` for a mode or variable it does not know.
pub fn vh_static_mode_method(mode: &str, variable: u8) -> Option<(&'static str, &'static str)> {
    vh_field_mode_method(mode, variable, false)
}

/// [`vh_static_mode_method`] for an INSTANCE field's handle (`instance`):
/// `VarHandle<T>s$FieldInstance{ReadOnly,ReadWrite}`, whose methods take the
/// holder as an `Object` after the `VarHandle`. j27 wave 9, lane W9EXC.
pub fn vh_field_mode_method(
    mode: &str,
    variable: u8,
    instance: bool,
) -> Option<(&'static str, &'static str)> {
    macro_rules! family {
        ($t:literal, $v:literal) => {
            if instance {
                (
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldInstanceReadOnly"),
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldInstanceReadWrite"),
                    [
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;)", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, ")V"),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, $v, ")Z"),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, $v, ")", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, ")", $v),
                    ],
                )
            } else {
                (
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldStaticReadOnly"),
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldStaticReadWrite"),
                    [
                        concat!("(Ljava/lang/invoke/VarHandle;)", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, ")V"),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, $v, ")Z"),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, $v, ")", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, ")", $v),
                    ],
                )
            }
        };
    }
    let (read_only, read_write, descriptors) = match variable {
        b'Z' => family!("Booleans", "Z"),
        b'B' => family!("Bytes", "B"),
        b'S' => family!("Shorts", "S"),
        b'C' => family!("Chars", "C"),
        b'I' => family!("Ints", "I"),
        b'J' => family!("Longs", "J"),
        b'F' => family!("Floats", "F"),
        b'D' => family!("Doubles", "D"),
        b'L' | b'[' => family!("References", "Ljava/lang/Object;"),
        _ => return None,
    };
    let shape = match mode {
        "get" | "getVolatile" | "getOpaque" | "getAcquire" => 0,
        "set" | "setVolatile" | "setOpaque" | "setRelease" => 1,
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => 2,
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => 3,
        m if m.starts_with("getAnd") => 4,
        _ => return None,
    };
    Some((
        if shape == 0 { read_only } else { read_write },
        descriptors[shape],
    ))
}

/// The served-raise chain of a static VarHandle's holder initialization
/// (JDK 25.0.3, `L4W59HandleClassInitFrames` rows `vh-static-*`): the caller's
/// `VarHandle.<mode>`, unlisted, then `VarHandle<T>s$FieldStatic*.<mode>` ->
/// `LazyInitializingVarHandle.target` -> `ensureInitialized` -> `initialize`
/// -> `Unsafe.ensureClassInitialized` -> `ensureClassInitialized0`. `None`
/// when the access is not known. Interpreter round i2 wave 59, lane L4.
pub fn vh_static_init_chain(site: VhStaticInit) -> Option<cratonvm_native_api::ServedRaiseChain> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const LAZY: &str = "java/lang/invoke/LazyInitializingVarHandle";
    const TARGET: &str = "()Ljava/lang/invoke/VarHandle;";
    const VOID: &str = "()V";
    let (class, descriptor) = vh_static_mode_method(site.mode, site.variable)?;
    let last = |name: &'static str, descriptor: &'static str| ServedRaiseHopSite::LastCall {
        name,
        descriptor: Some(descriptor),
    };
    let mut hops = vec![
        ServedRaiseHop {
            class: "java/lang/invoke/VarHandle",
            name: site.mode,
            descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
            site: ServedRaiseHopSite::Hidden,
        },
        ServedRaiseHop {
            class,
            name: site.mode,
            descriptor,
            site: last("target", TARGET),
        },
        ServedRaiseHop {
            class: LAZY,
            name: "target",
            descriptor: TARGET,
            site: last("ensureInitialized", VOID),
        },
        ServedRaiseHop {
            class: LAZY,
            name: "ensureInitialized",
            descriptor: VOID,
            site: last("initialize", VOID),
        },
        ServedRaiseHop {
            class: LAZY,
            name: "initialize",
            descriptor: VOID,
            site: last("ensureClassInitialized", "(Ljava/lang/Class;)V"),
        },
    ];
    hops.extend(crate::lang_class::class_init_chain_tail(
        "jdk/internal/misc/Unsafe",
    ));
    Some(ServedRaiseChain {
        throwable_class: "java/lang/ExceptionInInitializerError",
        hops,
    })
}

/// The served-raise chain of an instance-field VarHandle access with a null
/// holder (JDK 25.0.3, `J27NpeHiddenRows` rows `vh-get-null-holder` /
/// `vh-set-null-holder`): the caller's `VarHandle.<mode>`, unlisted (with the
/// hidden `VarHandleGuards.guard_*`), then `VarHandle<T>s$FieldInstance*.<mode>`
/// at its `Objects.requireNonNull(holder)` -> `Objects.requireNonNull`, whose
/// frame is the trace's top. `None` when the access is not known. j27 wave 9,
/// lane W9EXC.
pub fn vh_null_holder_chain(
    mode: &str,
    variable: u8,
) -> Option<cratonvm_native_api::ServedRaiseChain> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const REQUIRE_NON_NULL: &str = "requireNonNull";
    const OBJECT_TO_OBJECT: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let mode = vh_access_mode_name(mode)?;
    let (class, descriptor) = vh_field_mode_method(mode, variable, true)?;
    Some(ServedRaiseChain {
        throwable_class: "java/lang/NullPointerException",
        hops: vec![
            ServedRaiseHop {
                class: "java/lang/invoke/VarHandle",
                name: mode,
                descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
                site: ServedRaiseHopSite::Hidden,
            },
            ServedRaiseHop {
                class,
                name: mode,
                descriptor,
                site: ServedRaiseHopSite::LastCall {
                    name: REQUIRE_NON_NULL,
                    descriptor: Some(OBJECT_TO_OBJECT),
                },
            },
            ServedRaiseHop {
                class: "java/util/Objects",
                name: REQUIRE_NON_NULL,
                descriptor: OBJECT_TO_OBJECT,
                site: ServedRaiseHopSite::Raise,
            },
        ],
    })
}

/// `mode` as the `'static` name of one of `VarHandle.AccessMode`'s 31 methods.
pub fn vh_access_mode_name(mode: &str) -> Option<&'static str> {
    const MODES: [&str; 31] = [
        "get", "set", "getVolatile", "setVolatile", "getAcquire", "setRelease", "getOpaque",
        "setOpaque", "compareAndSet", "compareAndExchange", "compareAndExchangeAcquire",
        "compareAndExchangeRelease", "weakCompareAndSetPlain", "weakCompareAndSet",
        "weakCompareAndSetAcquire", "weakCompareAndSetRelease", "getAndSet", "getAndSetAcquire",
        "getAndSetRelease", "getAndAdd", "getAndAddAcquire", "getAndAddRelease",
        "getAndBitwiseOr", "getAndBitwiseOrRelease", "getAndBitwiseOrAcquire",
        "getAndBitwiseAnd", "getAndBitwiseAndRelease", "getAndBitwiseAndAcquire",
        "getAndBitwiseXor", "getAndBitwiseXorRelease", "getAndBitwiseXorAcquire",
    ];
    MODES.iter().copied().find(|&m| m == mode)
}

/// Re-read `live` through the pins [`pin_mh_args`] took, then release them.
pub fn vh_refresh_live(
    ctx: &mut dyn NativeContext,
    base: usize,
    handles: &[usize],
    live: &mut [Value],
) {
    for (v, h) in live.iter_mut().zip(handles.iter()) {
        *v = read_pinned_mh_arg(&*ctx, *h, *v);
    }
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
}

/// Witness comparison used by VarHandle `compareAndSet` / `compareAndExchange`:
/// objects by identity (pointer), primitives by bit pattern.
pub fn vh_values_match(current: &Value, expected: &Value) -> bool {
    match (current, expected) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
        (Value::Object(a), Value::Object(b)) => match (a, b) {
            (Some(ra), Some(rb)) => ra.as_ptr() == rb.as_ptr(),
            (None, None) => true,
            _ => false,
        },
        _ => false,
    }
}

/// LOW-finding fix (VarHandle RMW atomicity): run `body` while holding the
/// monitor of `cid`'s class mirror so that read-modify-write VarHandle
/// operations on a *static* field are linearizable w.r.t. each other.
///
/// Instance- and array-element RMWs use the VM's `compare_and_swap_field`
/// (a real per-object CAS lock), but the trait exposes no CAS primitive for
/// static slots, so we serialize them on the per-class mirror object — a
/// stable, unique lock shared by every VarHandle targeting that class's
/// statics (`get_class_mirror` returns the same `Class` object each call).
/// JIT round 21 (lane staticcas): the trait now has one
/// (`compare_and_exchange_static_field`, a hardware CAS), and every caller's
/// body uses it, so the monitor no longer carries the atomicity (it never
/// excluded `putstatic` or the `Unsafe` natives); dropping it is proposal
/// STATICCAS21-1. The monitor is always released (the closure here only does infallible
/// `Value` get/set, so it cannot unwind, but we keep enter/exit balanced).
///
/// `monitor_enter` can block, and a blocked thread is a GC point: the mirror
/// and the caller's operands (`live`: the witness, the value to store) are
/// pinned across it and `body` receives the current operands. The mirror used
/// to be handed to `monitor_exit` and the operands stored from their
/// pre-block addresses (wave 17).
///
/// The operands are pinned BEFORE the mirror is fetched: `get_class_mirror`
/// mints a class's mirror on first use (`get_or_create_class_mirror`), and
/// that allocation is a GC point too. The declaring class of an INHERITED
/// static (`vh_static_slot`'s wave-16 arm) is the likeliest class never to
/// have had one. The operands used to be pinned only after it, from
/// addresses that allocation could already have moved (gc-common w33-b).
pub fn vh_with_static_lock<R>(
    ctx: &mut dyn NativeContext,
    cid: ClassId,
    live: &[Value],
    body: impl FnOnce(&mut dyn NativeContext, &[Value]) -> R,
) -> R {
    let (live_base, handles) = pin_mh_args(ctx, live);
    // JIT round 23 wave 2, lane L6 (`CRATONVM_STATIC_RMW_VM_LOCK`, default
    // ON): a VM-private lock per class instead of the mirror's Java monitor,
    // which `synchronized (Foo.class)` shares and thread dumps show; HotSpot's
    // static RMWs never touch it. The operands stay pinned across the wait
    // (a blocking region may see a moving GC) and are re-read under the lock.
    if let Some(guard) = crate::lang_class::static_rmw_lock(ctx, cid) {
        let fresh = read_pinned_mh_args(&*ctx, &handles, live);
        let r = body(&mut *ctx, &fresh);
        drop(guard);
        if live_base != usize::MAX {
            ctx.unpin_native_roots(live_base);
        }
        return r;
    }
    let lock = ctx.get_class_mirror(cid);
    let lock_pin = ctx.pin_native_root(lock);
    ctx.monitor_enter(lock);
    let lock = ctx.read_native_pin(lock_pin, lock);
    let fresh = read_pinned_mh_args(&*ctx, &handles, live);
    // Reborrow so `ctx` is still usable for `monitor_exit` after the closure
    // (a bare `body(ctx)` would move the `&mut dyn` reference).
    let r = body(&mut *ctx, &fresh);
    ctx.monitor_exit(lock);
    // Every read is done: release the OLDEST pin of this group once (the
    // first operand's when one was pinned, else the mirror's), which truncates
    // every newer one. `live_base` is `usize::MAX` when no operand was pinned.
    ctx.unpin_native_roots(live_base.min(lock_pin));
    r
}

/// Read class+field names for a STATIC VarHandle: prefer the meta side table,
/// fall back to the synthetic VH_CLASS / VH_FIELD string slots.
/// The class a static `VarHandle` was minted against, when strict mode recorded it
/// (`alloc_static_var_handle`); `None` means resolve the name, as before.
pub fn vh_static_owner(meta: Option<&VarHandleMeta>) -> Option<ClassId> {
    meta.filter(|m| m.class_id != 0)
        .map(|m| ClassId::new(m.class_id))
}

pub fn vh_static_class_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
) -> (String, String) {
    match meta {
        Some(m) => (m.class_name.clone(), m.field_name.clone()),
        None => (
            vh_read_string(ctx, this, VH_CLASS).unwrap_or_default(),
            vh_read_string(ctx, this, VH_FIELD).unwrap_or_default(),
        ),
    }
}

/// The slot of an INSTANCE `VarHandle`'s field, resolved by name for a handle
/// minted before its field could be resolved (`field_index == -1`): in the
/// field's holder ([`VarHandleMeta::holder_class_id`]) by class id, else in
/// `class_name` by name, as before (wave 46, lane L4).
pub fn vh_instance_field_by_name(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
) -> Option<usize> {
    match meta {
        Some(m) if m.holder_class_id != 0 => {
            ctx.resolve_field_index_by_class_id(ClassId::new(m.holder_class_id), &m.field_name)
        }
        Some(m) => ctx.resolve_field_index(&m.class_name, &m.field_name),
        None => {
            let class = vh_read_string(ctx, this, VH_CLASS).unwrap_or_default();
            let field = vh_read_string(ctx, this, VH_FIELD).unwrap_or_default();
            ctx.resolve_field_index(&class, &field)
        }
    }
}

/// Read VarHandle metadata helpers.
pub fn vh_read_string(ctx: &mut dyn NativeContext, vh: ObjectRef, field: usize) -> Option<String> {
    match ctx.get_field(vh, field) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Byte width of a byte-array-view element descriptor.
pub fn byte_view_width(elem: u8) -> usize {
    match elem {
        b'J' | b'D' => 8,
        b'I' | b'F' => 4,
        b'S' | b'C' => 2,
        _ => 1,
    }
}

pub fn byte_view_desc(elem: u8) -> &'static str {
    match elem {
        b'J' => DESC_LONG,
        b'D' => DESC_DOUBLE,
        b'I' => DESC_INT,
        b'F' => DESC_FLOAT,
        b'S' => DESC_SHORT,
        b'C' => DESC_CHAR,
        b'B' => DESC_BYTE,
        b'Z' => DESC_BOOLEAN,
        _ => DESC_BYTE,
    }
}

pub fn array_element_desc(ctx: &dyn NativeContext, arr: ObjectRef) -> &'static str {
    match ctx.heap_element_type_of(arr) {
        ArrayElementType::Boolean => DESC_BOOLEAN,
        ArrayElementType::Byte => DESC_BYTE,
        ArrayElementType::Char => DESC_CHAR,
        ArrayElementType::Short => DESC_SHORT,
        ArrayElementType::Int => DESC_INT,
        ArrayElementType::Long => DESC_LONG,
        ArrayElementType::Float => DESC_FLOAT,
        ArrayElementType::Double => DESC_DOUBLE,
        ArrayElementType::Reference => DESC_REF,
    }
}

pub fn vh_access_type_ordinal(ctx: &mut dyn NativeContext, access_type: ObjectRef) -> i32 {
    if let Value::Int(v) = ctx.get_field(access_type, 1) {
        return v;
    }
    if let Value::Int(v) = ctx.get_field_by_name(access_type, "ordinal") {
        return v;
    }
    match ctx.invoke_virtual(access_type, "ordinal", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        _ => 0,
    }
}

pub fn vh_width_descriptor(width: i32) -> &'static str {
    match width {
        1 => DESC_BYTE,
        2 => DESC_SHORT,
        4 => DESC_INT,
        8 => DESC_LONG,
        _ => DESC_OBJECT,
    }
}

pub fn vh_sanitize_value_descriptor(desc: &str) -> Cow<'static, str> {
    if desc == DESC_REF || desc.is_empty() {
        Cow::Borrowed(DESC_OBJECT)
    } else if desc.len() == 1
        || desc.starts_with('[')
        || (desc.starts_with('L') && desc.ends_with(';'))
    {
        Cow::Owned(desc.to_string())
    } else {
        Cow::Borrowed(DESC_OBJECT)
    }
}

pub fn vh_access_mode_descriptor(access_type: i32, coords: &[String], value_desc: &str) -> String {
    let value_desc = vh_sanitize_value_descriptor(value_desc);
    let mut params = String::new();
    for coord in coords {
        params.push_str(coord);
    }
    let mut desc = String::from("(");
    match access_type {
        1 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(DESC_VOID);
        }
        2 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(DESC_BOOLEAN);
        }
        3 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(&value_desc);
        }
        4 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(&value_desc);
        }
        _ => {
            desc.push_str(&params);
            desc.push(')');
            desc.push_str(&value_desc);
        }
    }
    desc
}

pub fn p67_memory_segment_varhandle_descriptor(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    access_type: i32,
) -> Option<String> {
    // The shape table names the CARRIER, which a width cannot: `JAVA_INT` and
    // `JAVA_FLOAT` are both four bytes and must type as `int`/`float`. It also
    // carries the index coordinate a sequence-element path added.
    if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
        return Some(vh_access_mode_descriptor(
            access_type,
            &layout_vh_coordinates(shape),
            layout_vh_carrier_desc(shape.carrier),
        ));
    }
    let slot_width = if ctx.get_field(vh, 2).as_int() == Some(3) {
        ctx.get_field(vh, 1).as_int()
    } else {
        None
    };
    if let Some(width) = slot_width {
        let coords = vec![
            "Ljava/lang/foreign/MemorySegment;".to_string(),
            DESC_LONG.to_string(),
        ];
        return Some(vh_access_mode_descriptor(
            access_type,
            &coords,
            vh_width_descriptor(width),
        ));
    }
    if is_segment_var_handle(ctx, vh) {
        // Round 13 wave 1 (lane mhffm): the form's value type and coordinate
        // count, as `SegmentVarHandle.accessModeTypeUncached` reads them
        // (`CRATONVM_FFM_SEGMENT_VH_FORM`); `enclosing` is a group layout for
        // a path handle.
        let form = if segment_vh_checks() && segment_vh_form_enabled() {
            segment_vh_form_shape(ctx, vh)
        } else {
            None
        };
        let (value_desc, variable) = match form {
            Some(shape) => (seg_shape_desc(shape), segment_vh_form_is_variable(ctx, vh)),
            None => (
                segment_vh_fields(ctx, vh)
                    .map(|(layout, _, _)| seg_shape_desc(segment_layout_shape(ctx, layout)))
                    .unwrap_or(DESC_LONG),
                false,
            ),
        };
        let mut coords = vec![
            "Ljava/lang/foreign/MemorySegment;".to_string(),
            DESC_LONG.to_string(),
        ];
        if variable {
            coords.push(DESC_LONG.to_string());
        }
        return Some(vh_access_mode_descriptor(access_type, &coords, value_desc));
    }
    None
}

/// What a CratonVM VarHandle addresses, as JVM descriptors: the variable type
/// and the coordinate types, plus whether the coordinates are the REAL ones.
///
/// Derived from the WP4.2 side table, falling back to the synthetic slots. One
/// function so `accessModeType` and `varType`/`coordinateTypes` can never
/// disagree about what a handle points at.
///
/// `coords_exact == false` means a coordinate had to be erased to
/// `java/lang/Object` because the metadata does not record the real class.
/// `accessModeType` tolerates that — an erased `MethodType` still dispatches —
/// but `coordinateTypes()` must NOT, because there its answer would be a
/// fabricated `Class` presented as fact.
///
/// `VH_KIND_ARRAY` used to be the standing example: its producer
/// (`MethodHandles.arrayElementVarHandle`) was registered only in
/// `phases_late/reflect_invoke.rs`, which does not run in real-JDK mode, so an
/// array handle reached here with no side-table entry and the leading
/// coordinate had to be erased. That factory is now registered on the live
/// path in this file and records the array class, so the erasure fires only
/// for a handle CratonVM did not mint — where refusing is the right answer.
/// Read a `Class`-typed field off a REAL JDK object and return its descriptor.
///
/// The `Object(Some(_))` match is what separates "the field is there and holds
/// a mirror" from "this is not the class I thought it was".
///
/// **Corrected 2026-08-07.** This used to say `get_field_by_name` answers
/// `Int(0)` for a field that does not exist. It does not. PRODUCTION
/// (`vm/src/vm/vm_exec.rs::get_field_by_name`) resolves the name in the
/// hierarchy and returns **`Value::Object(None)`** when it cannot — the
/// `Int(0)` is `MockNativeContext`'s (`native-builtins/src/test_utils.rs`) and
/// nobody else's; `native-api/src/test_mock.rs` agrees with production. The
/// code here is right either way (both `Object(None)` and `Int(0)` fall to the
/// `_` arm), but the reasoning was not, and the sharper hazard the old note
/// hid is that an absent field is INDISTINGUISHABLE from a real null: a
/// `matches!(…, Value::Object(_))` layout discriminator matches BOTH. Never
/// write one.
pub fn vh_real_class_field_desc(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Option<String> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(mirror)) => Some(mirror_to_descriptor(ctx, mirror).into_owned()),
        _ => None,
    }
}

/// `(element descriptor, coordinate descriptors)` for a REAL-JDK array-element
/// VarHandle — the object `MethodHandles.arrayElementVarHandle` hands back when
/// its own bytecode runs, which is what happens in `--fast-boot`/`--real-jdk`
/// (this file registers no native for that factory; see the note next to
/// `byteArrayViewVarHandle`). `None` for anything else.
///
/// Such a handle carries no WP4.2 side-table entry, so without this the
/// describe-yourself accessors refuse — and they are FORCED to run by the
/// `check_override` entry in `vm/src/vm/vm_exec.rs`, which pins
/// `VarHandle.varType`/`coordinateTypes` to the registry for every receiver.
/// The refusal would therefore replace an answer the real bytecode could have
/// given, which is a worse outcome than the gap it was written for.
///
/// The real object knows its own type; the only question is where. Measured on
/// JDK 25, `MethodHandles.arrayElementVarHandle(T[].class).getClass()`:
///
/// ```text
/// int[]      VarHandleInts$Array       varType int      coords [class [I, int]
/// long[]     VarHandleLongs$Array      varType long     coords [class [J, int]
/// byte[]     VarHandleBytes$Array      varType byte     coords [class [B, int]
/// short[]    VarHandleShorts$Array     varType short    coords [class [S, int]
/// char[]     VarHandleChars$Array      varType char     coords [class [C, int]
/// float[]    VarHandleFloats$Array     varType float    coords [class [F, int]
/// double[]   VarHandleDoubles$Array    varType double   coords [class [D, int]
/// boolean[]  VarHandleBooleans$Array   varType boolean  coords [class [Z, int]
/// String[]   VarHandleReferences$Array varType String   coords [class [Ljava.lang.String;, int]
/// int[][]    VarHandleReferences$Array varType [I       coords [class [[I, int]
/// ```
///
/// Two facts drive the shape. `varType()` is the COMPONENT type, not the array
/// type; and there are TWO coordinates, `{arrayClass, int}`, where a field
/// handle has one. The primitive families encode the element type in the class
/// NAME and declare only `abase`/`ashift`; `References$Array` is the sole
/// family that cannot, and it declares `arrayType`/`componentType` `Class`
/// fields instead (`javap -p`) — which is why it is the one arm that reads
/// fields rather than parsing a name.
pub fn real_array_var_handle_descriptors(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
) -> Option<(String, Vec<String>)> {
    let class_id = ctx.class_id_of_object(vh);
    let name = ctx.class_name_of_id(class_id)?;
    let family = name.strip_prefix("java/lang/invoke/VarHandle")?;
    let elem = match family {
        "Ints$Array" => DESC_INT,
        "Longs$Array" => DESC_LONG,
        "Bytes$Array" => DESC_BYTE,
        "Shorts$Array" => DESC_SHORT,
        "Chars$Array" => DESC_CHAR,
        "Floats$Array" => DESC_FLOAT,
        "Doubles$Array" => DESC_DOUBLE,
        "Booleans$Array" => DESC_BOOLEAN,
        "References$Array" => {
            // Both halves or neither: a coordinate list without a variable
            // type (or the reverse) would let one accessor answer while the
            // other invents.
            let array_desc = vh_real_class_field_desc(ctx, vh, "arrayType")?;
            let component_desc = vh_real_class_field_desc(ctx, vh, "componentType")?;
            return Some((component_desc, vec![array_desc, DESC_INT.to_string()]));        }
        _ => return None,
    };
    Some((
        elem.to_string(),
        vec![format!("[{elem}"), DESC_INT.to_string()],
    ))
}

/// `LambdaMetafactory.metafactory`'s descriptor.
pub const LMF_METAFACTORY_DESC: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;";
/// `LambdaMetafactory.altMetafactory`'s descriptor.
pub const LMF_ALT_METAFACTORY_DESC: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;";

/// A direct (reflective) `LambdaMetafactory.<method>` call under
/// `CRATONVM_LMF_REFLECTIVE_TO_JDK` (interpreter round i1 wave 49, lane L4;
/// `i48-L4-proposal-retire-the-reflective-lambdametafactory-bridge`): the
/// JDK's own factory body, past this registration, as HotSpot runs it. Its
/// checks raise the JDK's `LambdaConversionException`s (the bridge linked a
/// non-interface target, a wrong arity, and panicked the VM on a
/// non-convertible return), and it spins the JDK's hidden proxy class. The
/// `invokedynamic` route never reaches the bridge; this is the reflective
/// callers' route (log4j's `ServiceLoaderUtil`, Spring's `LambdaSafe`).
/// `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints one `[lmf-reflective]` line per
/// call (the positive control, probe `L4W49ReflectiveMetafactory`).
#[cold]
#[inline(never)]
pub fn lmf_reflective_to_jdk(
    ctx: &mut dyn NativeContext,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    if false {
        eprintln!("[lmf-reflective] LambdaMetafactory.{method} -> JDK factory");    }
    ctx.invoke_special_bytecode_only(
        "java/lang/invoke/LambdaMetafactory",
        method,
        descriptor,
        args,
    )
}

/// The JDK's own argument checks for the reflective bridge (switch off): the
/// `InnerClassLambdaMetafactory` constructor and `validateMetafactoryArgs()`
/// that `LambdaMetafactory.metafactory` runs before `buildCallSite()`, which
/// alone spins the proxy class. A `LambdaConversionException` they throw is
/// the call's (`java.lang.String is not an interface`, a wrong arity, a
/// non-adaptable type); any other failure of theirs (a handle of this VM's
/// model that `revealDirect` cannot crack) is not, and the bridge links as it
/// did. Interpreter round i1 wave 50, lane L4 (`L4W49ReflectiveMetafactory`
/// rows `not-interface` and `arity` linked a `ConstantCallSite`).
///
/// `args` are the factory's (`altMetafactory`'s `samType`, `implementation`,
/// `instantiatedType` and flags come out of its `Object[]`; its markers and
/// bridges are not passed: their checks are the JDK's `FLAG_MARKERS` loop
/// over classes the caller already holds). Answers a copy of `args` read
/// back after the checks, which may collect: the slice itself is stale then.
#[cold]
#[inline(never)]
pub fn lmf_bridge_jdk_checks(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    alt: bool,
) -> Result<Vec<Value>, MethodCallFailed> {
    let obj = |v: Option<&Value>| match v {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let mut objs = [None; 6];
    for (i, slot) in objs.iter_mut().enumerate().take(3) {
        *slot = obj(args.get(i));
    }
    let mut serializable = false;
    if alt {
        if let Some(arr) = obj(args.get(3)) {
            let len = ctx.array_length(arr);
            for k in 0..3.min(len) {
                objs[3 + k] = match ctx.get_array_element(arr, k) {
                    Value::Object(o) => o,
                    _ => None,
                };
            }
            if len > 3 {
                let flags = match ctx.get_array_element(arr, 3) {
                    Value::Int(i) => i,
                    Value::Object(Some(b)) => {
                        ctx.get_field_by_name(b, "value").as_int().unwrap_or(0)                    }
                    _ => 0,
                };
                serializable = flags & 0x1 != 0;            }        }    } else {
        for (i, slot) in objs.iter_mut().enumerate().skip(3) {
            *slot = obj(args.get(i));
        }
    }
    // The JDK's own null checks are `Objects.requireNonNull` in the factory;
    // the bridge keeps its own answer for a null argument.
    if objs.iter().any(Option::is_none) {
        return Ok(args.to_vec());
    }
    let arg_pins: Vec<Option<usize>> = args
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        })
        .collect();
    let pins: Vec<usize> = objs
        .iter()
        .flatten()
        .map(|o| ctx.pin_native_root(*o))
        .collect();
    let base = arg_pins.iter().flatten().next().copied().unwrap_or(pins[0]);
    let result = lmf_bridge_jdk_checks_run(ctx, objs, &pins, serializable);
    let fresh: Vec<Value> = args
        .iter()
        .zip(arg_pins.iter())
        .map(|(v, pin)| match (v, pin) {
            (Value::Object(Some(o)), Some(p)) => Value::Object(Some(ctx.read_native_pin(*p, *o))),
            _ => *v,
        })
        .collect();
    ctx.unpin_native_roots(base);
    match result {
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let is_lce = ctx
                .class_name_of_id(ctx.class_id_of_object(exc))
                .is_some_and(|n| n == "java/lang/invoke/LambdaConversionException");
            if is_lce {
                Err(MethodCallFailed::ExceptionThrown(exc))
            } else {
                Ok(fresh)
            }
        }
        _ => Ok(fresh),
    }
}

/// The body of [`lmf_bridge_jdk_checks`]: `objs` (`[caller, invokedName,
/// factoryType, samType, implementation, instantiatedType]`) are pinned at
/// `pins`.
pub fn lmf_bridge_jdk_checks_run(
    ctx: &mut dyn NativeContext,
    objs: [Option<ObjectRef>; 6],
    pins: &[usize],
    serializable: bool,
) -> Result<(), MethodCallFailed> {
    const ICLMF: &str = "java/lang/invoke/InnerClassLambdaMetafactory";
    const CTOR: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/invoke/MethodType;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;Z[Ljava/lang/Class;[Ljava/lang/invoke/MethodType;)V";
    {
        let (Some(class_cid), Some(mt_cid)) = (
            ctx.class_id_by_name("java/lang/Class"),
            ctx.class_id_by_name("java/lang/invoke/MethodType"),
        ) else {
            return Ok(());
        };
        let no_classes = ctx.new_ref_array(class_cid, 0);
        let no_classes_pin = ctx.pin_native_root(no_classes);
        let no_types = ctx.new_ref_array(mt_cid, 0);
        let no_classes = ctx.read_native_pin(no_classes_pin, no_classes);
        let o: Vec<ObjectRef> = objs
            .iter()
            .flatten()
            .enumerate()
            .map(|(i, o)| ctx.read_native_pin(pins[i], *o))
            .collect();
        // Constructor order: caller, factoryType, name, samType, impl, inst.
        let mf = match ctx.new_object_initialized(
            ICLMF,
            CTOR,
            &[
                Value::Object(Some(o[0])),
                Value::Object(Some(o[2])),
                Value::Object(Some(o[1])),
                Value::Object(Some(o[3])),
                Value::Object(Some(o[4])),
                Value::Object(Some(o[5])),
                Value::Int(serializable as i32),
                Value::Object(Some(no_classes)),
                Value::Object(Some(no_types)),
            ],
        )? {
            Some(Value::Object(Some(mf))) => mf,
            _ => return Ok(()),
        };
        ctx.invoke_virtual(mf, "validateMetafactoryArgs", "()V", &[])?;
        Ok(())
    }
}

/// `VarHandle.withInvokeExactBehavior()` (`exact == true`) and
/// `withInvokeBehavior()` (`false`), every mode since wave 49 (interpreter
/// round i1 wave 37, lane L4; item 3 of
/// `interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`): the
/// handle itself when it already has that behaviour, otherwise a NEW handle
/// over the same variable with it, as the JDK's builders answer.
///
/// * A handle CratonVM minted: the copy has the plain handle's side-table row
///   with [`VarHandleMeta::exact`] set (the five access mode descriptors,
///   computed here once) or cleared, and its real `exact` field set, which is
///   what `hasInvokeExactBehavior()` (JDK bytecode) reads.
/// * The JDK's own array-element handles (`arrayElementVarHandle` runs JDK
///   bytecode, so they have no row): their concrete builder, which this
///   registration stands in front of.
///
/// Minting an exact handle publishes the slot of `VarHandle.exact` to the VM
/// (`note_exact_var_handle_minted`), which turns the doors' check on
/// ([`varhandle_exact_call_site_refusal`]) early (the doors also resolve
/// it themselves since interpreter wave 49). `this` unchanged for any other
/// handle.
///
/// `--compatible` (JIT round 15 wave 1, lane vh;
/// `r15w1-vh-compatible-mode-ignores-invoke-exact-behavior-FIXED-20260930.md`): the
/// same answers, so `hasInvokeExactBehavior()` and the call-site refusal
/// match HotSpot there too. Safe to open the doors' gate in that mode: a
/// real-JDK `VarHandle` has the real layout in both policies, and the gate is
/// resolved from a real `exact` field or not opened at all (a synthetic-layout
/// copy has none), and a receiver the gate misreads is only asked
/// [`varhandle_exact_call_site_refusal`], which accepts every non-exact
/// handle. `CRATONVM_VH_EXACT_COMPATIBLE=0` restores `this` in that mode.
/// Interpreter round i1 wave 49 (lane L4) made the same change independently
/// (`L4W48VarHandleExactBehavior` row `field-cas-boxed`: a field handle's
/// `withInvokeExactBehavior()` converted as a plain one in `--compatible`).
pub fn varhandle_with_exactness(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    exact: bool,
) -> MethodCallResult {
    let this = args.first().copied().unwrap_or(Value::Object(None));
    let Value::Object(Some(vh)) = this else {
        return Ok(Some(this));
    };
    // Every mode by default (JIT round 15 wave 1, lane vh; interpreter wave
    // 49, lane L4): `--compatible` answered `this`, so a field handle's
    // `withInvokeExactBehavior()` converted as a plain one
    // (`L4W48VarHandleExactBehavior` row `field-cas-boxed`).
    // `CRATONVM_VH_EXACT_COMPATIBLE=0` restores that answer in `--compatible`.
    if !vh_exact_judged_in_this_mode(ctx) {
        return Ok(Some(this));
    }
    let Some(meta) = vh_meta_get(ctx, vh) else {
        // An `IndirectVarHandle` reached through `VarHandle`'s registration:
        // its own builder (real bytecode), not `this` (round 15 wave 1, lane
        // vh; `r14w7-ffm7-indirect-var-handle-residuals` item 1).
        if vh_indirect_exact_enabled() && is_indirect_var_handle(ctx, vh) {
            let method = if exact {
                "withInvokeExactBehavior"
            } else {
                "withInvokeBehavior"
            };
            let built = ctx.invoke_virtual_bytecode_only(
                vh,
                method,
                "()Ljava/lang/invoke/VarHandle;",
                &[],
            )?;
            indirect_vh_note_exact(ctx, &built);
            return Ok(built);
        }
        // An FFM layout handle (`ValueLayout.varHandle()`): a copy over the
        // same layout shape with the other flag, which is all it carries: an
        // exact one is judged from its shape and its real `exact` field
        // (`varhandle_exact_call_site_refusal`, wave 38), and its mint opens
        // the doors' gate like any other exact handle's.
        if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
            let now_exact = matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0);
            if now_exact == exact {
                return Ok(Some(this));
            }
            let copy =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
            if !vh_has_synthetic_layout(ctx, copy) {
                let class_id = ctx.class_id_of_object(copy);
                if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "exact") {
                    ctx.set_field(copy, slot, Value::Int(i32::from(exact)));
                    if exact {
                        ctx.note_exact_var_handle_minted(slot);
                    }
                }
            }
            register_p67_memory_segment_var_handle(ctx, copy, shape);
            return Ok(Some(Value::Object(Some(copy))));
        }
        if real_array_var_handle_descriptors(ctx, vh).is_none() {
            return Ok(Some(this));
        }
        let method = if exact {
            "withInvokeExactBehavior"
        } else {
            "withInvokeBehavior"
        };
        let built =
            ctx.invoke_virtual_bytecode_only(vh, method, "()Ljava/lang/invoke/VarHandle;", &[])?;
        if exact {
            if let Some(Value::Object(Some(handle))) = built {
                let class_id = ctx.class_id_of_object(handle);
                if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "exact") {
                    ctx.note_exact_var_handle_minted(slot);
                }
            }
        }
        return Ok(built);
    };
    if meta.exact.is_some() == exact {
        return Ok(Some(this));
    }
    // The access mode types, from the plain handle's own description, before
    // the allocation below (`vh` is not used after it).
    let types: Option<Arc<[String; 5]>> = if exact {
        let (_kind, value_desc, coords, coords_exact) =
            vh_value_and_coordinate_descriptors(ctx, vh);
        Some(Arc::new(std::array::from_fn(|access_type| {
            if coords_exact {
                vh_access_mode_descriptor(access_type as i32, &coords, &value_desc)
            } else {
                String::new()
            }
        })))
    } else {
        None
    };
    let copy = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
    // Real layout only: on the synthetic one slot 1 is `VH_CLASS`.
    let slot = if vh_has_synthetic_layout(ctx, copy) {
        None
    } else {
        let class_id = ctx.class_id_of_object(copy);
        ctx.resolve_field_index_by_class_id(class_id, "exact")
    };
    if let Some(slot) = slot {
        ctx.set_field(copy, slot, Value::Int(i32::from(exact)));
    }
    let mut row = (*meta).clone();
    row.exact = types;
    vh_meta_put(ctx, copy, row);
    if exact {
        if let Some(slot) = slot {
            ctx.note_exact_var_handle_minted(slot);
        }
    }
    Ok(Some(Value::Object(Some(copy))))
}

/// The access type (`VarHandle.AccessType` ordinal, as
/// [`vh_access_mode_descriptor`] numbers it) of a signature-polymorphic
/// access-mode method, or `None` for any other name.
pub fn vh_access_type_of_method(name: &str) -> Option<i32> {
    Some(match name {
        "get" | "getVolatile" | "getOpaque" | "getAcquire" => 0,
        "set" | "setVolatile" | "setOpaque" | "setRelease" => 1,
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => 2,
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => 3,
        _ if name.starts_with("getAndSet")
            || name.starts_with("getAndAdd")
            || name.starts_with("getAndBitwise") =>
        {
            4
        }
        _ => return None,
    })
}

/// `VarHandle.checkAccessModeThenIsDirect`'s refusal: `vh` has invoke-exact
/// behaviour and `call_site`, the descriptor the calling instruction names
/// for `method_name`, is not exactly the access mode type, so the access is
/// `WrongMethodTypeException("handle's method type (H)int but found
/// (H)long")`, raised before anything is read or written. `None` when the
/// handle is not exact, the types agree, or the handle cannot describe itself
/// exactly (every way of not knowing is an accept, as in
/// `exact_call_site_refusal`).
///
/// A handle CratonVM minted is judged by its side-table row
/// (`VarHandleMeta::exact`, precomputed: no allocation per access); the JDK's
/// own array-element handles by their real `exact` field and their class, an
/// FFM layout handle by its real `exact` field and its `p67` shape
/// (`(MemorySegment, long[, long])` and the layout's carrier; interpreter
/// round i1 wave 38, lane L4: before it every layout handle was declined, so
/// a mistyped access to an exact one converted). A layout handle on the
/// synthetic `VarHandle` layout is not judged: its slots hold the mint's
/// width, not `exact`.
///
/// For the doors that hold the REAL call-site descriptor and have already
/// seen the receiver's `exact` field set (interpreter round i1 wave 37, lane
/// L4): the interpreter's `execute_invoke_kind` and the JIT's site-cached
/// native dispatch.
pub fn varhandle_exact_call_site_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    method_name: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    let access_type = vh_access_type_of_method(method_name)?;
    let meta = vh_meta_get(ctx, vh);
    let computed: String;
    let expected: &str = match meta.as_deref() {
        Some(row) => row.exact.as_deref()?.get(access_type as usize)?.as_str(),
        None => {
            if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
                if vh_has_synthetic_layout(ctx, vh)
                    || !matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0)
                {
                    return None;
                }
                let layout_type = vh_access_mode_descriptor(
                    access_type,
                    &layout_vh_coordinates(shape),
                    layout_vh_carrier_desc(shape.carrier),
                );
                return varhandle_exact_mismatch(ctx, &layout_type, call_site);
            }
            // An exact `IndirectVarHandle` (round 15 wave 1, lane vh): its
            // own `value` / `coordinates`, the fields `accessModeTypeUncached`
            // reads on HotSpot.
            if let Some(indirect_type) = indirect_vh_exact_access_descriptor(ctx, vh, access_type) {
                return varhandle_exact_mismatch(ctx, &indirect_type, call_site);
            }
            // An exact REAL `SegmentVarHandle` (what `ValueLayout.varHandle()`
            // answers since SBR round 3 retired the FFM layouts under
            // `--real-jdk`; interpreter round i2 wave 55, lane L4, host run of
            // `L4W38FfmExactLayoutHandle`): it is neither in the synthetic
            // layout table above nor an array handle, so every mistyped access
            // was admitted and converted.
            if let Some(segment_type) = segment_vh_exact_access_descriptor(ctx, vh, access_type) {
                return varhandle_exact_mismatch(ctx, &segment_type, call_site);
            }
            let (value_desc, coords) = real_array_var_handle_descriptors(ctx, vh)?;
            if !matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0) {
                return None;
            }
            computed = vh_access_mode_descriptor(access_type, &coords, &value_desc);
            computed.as_str()
        }
    };
    varhandle_exact_mismatch(ctx, expected, call_site)
}

/// The access mode type of an exact real `java.lang.invoke.SegmentVarHandle`
/// for `access_type`, as its `accessModeTypeUncached` builds it: coordinates
/// `(MemorySegment, long)` (plus a `long` when the form's get type takes three
/// parameters, a variable offset), the value the form's erased get type
/// returns. `None` for any other handle, or one without invoke-exact
/// behaviour. Reads only.
pub fn segment_vh_exact_access_descriptor(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    access_type: i32,
) -> Option<String> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        != Some("java/lang/invoke/SegmentVarHandle")
    {
        return None;
    }
    if !matches!(
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "exact"),
        Value::Int(v) if v != 0
    ) {
        return None;
    }
    let (coords, value) = segment_vh_shape(ctx, vh)?;
    Some(vh_access_mode_descriptor(access_type, &coords, &value))
}

/// A real `java.lang.invoke.SegmentVarHandle`'s coordinate descriptors and
/// variable descriptor, from its form's erased get type (see
/// [`segment_vh_exact_access_descriptor`]); `None` for any other handle.
/// Reads only. Wave 57 (lane L4) split it out for the return-direction check
/// ([`vh_return_site_check`]) and its refusal record ([`vh_astype_refused`]).
pub fn segment_vh_shape(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(Vec<String>, String)> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        != Some("java/lang/invoke/SegmentVarHandle")
    {
        return None;
    }
    let get_desc = match real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform") {
        Value::Object(Some(vform)) => {
            match real_object_field(ctx, vform, "java/lang/invoke/VarForm", "methodType_table") {
                Value::Object(Some(table))
                    if ctx.object_is_array(table) && ctx.array_length(table) > 0 =>
                {
                    match ctx.get_array_element(table, 0) {
                        Value::Object(Some(get_type)) => {
                            descriptor_from_method_type(&*ctx, get_type)
                        }
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            }
        }
        _ => String::new(),
    };
    let parsed = split_descriptor_params(&get_desc);
    // Some host-JDK VarForm layouts expose the implementation class but not
    // the lazily populated MethodType table to the VM's field reader. The
    // concrete implementation class still determines the value carrier, and
    // SegmentVarHandle access modes have two or three coordinates. Keep the
    // MethodType path authoritative when it is available.
    let (value, variable) = if let Some((params, ret)) = parsed {
        (ret, params.len() == 3)
    } else {
        let shape = segment_vh_form_shape(ctx, vh)?;
        let value = match segment_vh_fields(ctx, vh) {
            Some((enclosing, _, _))
                if ctx
                    .class_name_of_id(ctx.class_id_of_object(enclosing))
                    .is_some_and(|name| name.contains("OfAddress")) =>
            {
                "Ljava/lang/foreign/MemorySegment;".to_string()
            }
            _ => seg_shape_desc(shape).to_string(),
        };
        (value, segment_vh_form_is_variable(ctx, vh))
    };
    let mut coords = vec![
        "Ljava/lang/foreign/MemorySegment;".to_string(),
        DESC_LONG.to_string(),
    ];
    if variable {
        coords.push(DESC_LONG.to_string());
    }
    Some((coords, value))
}

/// The real JDK `SegmentVarHandle`'s variable descriptor and coordinates,
/// decoded from its `VarForm`. Compatible mode can return this concrete JDK
/// handle directly from `ValueLayout.varHandle()`, so unlike a CratonVM-minted
/// layout handle it has no row in `P67_MEMORY_SEGMENT_VH_TABLE`.
pub fn real_segment_vh_descriptors(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
) -> Option<(String, Vec<String>)> {
    let (coordinates, variable) = segment_vh_shape(ctx, vh)?;
    Some((variable, coordinates))
}

