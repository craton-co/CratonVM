#[allow(unused_imports)]
use super::*;

/// Interpreter round i3 wave 80, lane L4 (`CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE`,
/// default off; proposal
/// `i73-L4-proposal-converting-invoke-of-a-real-field-accessor-is-served-from-its-record`):
/// `invoke` on a REAL field GETTER (`DirectMethodHandle$Accessor` /
/// `$StaticAccessor`, `--real-jdk`) at a site that differs from its type only
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
pub fn mh_real_field_converting_serve(
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

pub fn real_direct_target_decode(
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
pub fn mh_interface_receiver_check_enabled() -> bool {
    true
}

/// HotSpot's `Interface.checkReceiver` refusal for `receiver` against the
/// interface `refc`, or `None` when it implements it (or `refc` is not an
/// interface, or the switch is off). See [`mh_interface_receiver_check_enabled`].
pub fn mh_interface_receiver_refusal(
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
        // java.lang.Object.apply(int)'` (`--real-jdk`, the by-name call on the
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
pub fn mh_interface_receiver_refusal_named(
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
    mh_interface_receiver_refusal(ctx, receiver, refc)}

/// The memoised [`RealHandleRecord`] of `this`, or `None` for a handle this model
/// does not decode (the caller takes its ordinary path). Pure reads; no GC point.
pub fn real_handle_record(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<std::rc::Rc<RealHandleRecord>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return real_handle_decode(ctx, this).map(std::rc::Rc::new);
    }
    let epoch = ctx.gc_address_epoch();
    let addr = this.as_ptr() as usize;
    if let Some(hit) = REAL_HANDLE_MEMO.with(|memo| memo.borrow_mut().get(vm, epoch, addr)) {
        return Some(hit);
    }
    let record = std::rc::Rc::new(real_handle_decode(ctx, this)?);
    REAL_HANDLE_MEMO.with(|memo| memo.borrow_mut().insert(vm, epoch, addr, record.clone()));
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
pub fn mh_real_direct_fast(
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
            // frames (`L3W58HandleClassInitFrames` `mh-findStatic`, `--real-jdk`).
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
        // host round 2 of `L4W64SpecialByOwnerId`): THIS is the `--real-jdk` route of a
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
pub fn mh_real_constructor_fast(
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
            ctx.gc_address_epoch(),
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
pub const CTOR_CAST_ADAPTER_SITE: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

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
pub fn mh_real_ctor_cast_adapter_fast(
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
/// Pure reads, remembered per thread by `(VM, address epoch, handle
/// address)` (`gc_address_epoch`, never the collection count: t1refl), declines too: the answer is a function of the handles' final
/// fields and of the instance class having been initialized, which does not
/// revert (a decline for an instance class not initialized yet is not
/// remembered). The declines matter: every `Method.invoke` of a one-argument
/// static method asks this from the interpreter's `invokeExact` push at the
/// same `(Object)Object` site, of a `Species_LL` adapter of its own.
pub fn ctor_cast_adapter_decode(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ClassId, ClassId, std::sync::Arc<str>)> {
    let key = (ctx.vm_identity(), ctx.gc_address_epoch(), this.as_ptr() as usize);
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
pub enum CtorCastAdapterDecline {
    /// Not the shape: final for the handle's life.
    Never,
    /// The instance class is not initialized yet.
    NotYet,
}

pub fn ctor_cast_adapter_decode_uncached(
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
pub fn ctor_cast_adapter_shape(
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
pub fn ctor_cast_names_param(cast: Option<&str>, param: &str) -> bool {
    match (cast, param.strip_prefix('L').and_then(|p| p.strip_suffix(';'))) {
        (Some(cast), Some(name)) => cast == name,
        _ => false,
    }
}

pub const CTOR_CAST_ADAPTER_MEMO_SLOTS: usize = 16;

pub type CtorCastAdapterLine = (CtorLaneKey, u32, u32, Option<std::sync::Arc<str>>);

thread_local! {
    /// [`ctor_cast_adapter_decode`]'s answers (interpreter round i9),
    /// direct-mapped by handle address; `None` in the last place is a
    /// remembered decline.
    pub static CTOR_CAST_ADAPTER_MEMO: std::cell::RefCell<[CtorCastAdapterLine; CTOR_CAST_ADAPTER_MEMO_SLOTS]> =
        const { std::cell::RefCell::new([const { ((0, 0, 0), 0, 0, None) }; CTOR_CAST_ADAPTER_MEMO_SLOTS]) };
}

/// [`ctor_lane_instantiate`] after `Invokers.checkCustomized(this)`'s count
/// ([`lane_check_customized`]; JIT round 22 wave 3, lane runtime): the due
/// call declines to the linker, which customizes the handle; a decline of the
/// funnel takes the count back. The count is written before `<init>` runs, as
/// `invokeExact_MT` counts before its `invokeBasic`.
pub fn ctor_lane_counted_instantiate(
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
pub fn ctor_lane_instantiate(
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

/// `(VM identity, address epoch, handle address)`: the soundness argument
/// of `REAL_HANDLE_MEMO` (nothing moves or is freed within one epoch).
pub type CtorLaneKey = (usize, u64, usize);

pub const CTOR_LANE_MEMO_SLOTS: usize = 16;

thread_local! {
    /// [`mh_real_constructor_fast`]'s decode per handle (wave 89, lane L3):
    /// `(key, instance class, <init> owner)`, direct-mapped by address.
    pub static CTOR_LANE_MEMO: std::cell::RefCell<[(CtorLaneKey, u32, u32); CTOR_LANE_MEMO_SLOTS]> =
        const { std::cell::RefCell::new([((0, 0, 0), 0, 0); CTOR_LANE_MEMO_SLOTS]) };
}

pub fn ctor_lane_memo_get(key: CtorLaneKey) -> Option<(ClassId, ClassId)> {
    let slot = (key.2 >> 3) & (CTOR_LANE_MEMO_SLOTS - 1);
    CTOR_LANE_MEMO.with(|m| {
        let (k, inst, owner) = m.borrow()[slot];
        (k == key && key.0 != 0).then(|| (ClassId::new(inst), ClassId::new(owner)))
    })
}

pub fn ctor_lane_memo_put(key: CtorLaneKey, inst: ClassId, owner: ClassId) {
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
/// `--real-jdk`): ~200 us per call, a SECOND `invoke()` of one handle included,
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
pub fn mh_real_ctor_widening_serve(
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
pub fn mh_lf_vmentry_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| true)
}

/// A `LambdaForm.vmentry` `MemberName` decoded: the spun (or interpreter-entry)
/// class's static method and its basic-type descriptor.
pub struct LfEntryRecord {
    pub class_id: ClassId,
    pub class: String,
    pub name: String,
    pub desc: String,
    /// First character of every parameter token (`L`, `[` -> reference).
    pub kinds: Vec<u8>,
    /// As [`RealDirectTarget::settled`].
    pub settled: std::cell::Cell<Option<ClassId>>,
    /// As [`RealDirectTarget::door_key`].
    pub door_key: cratonvm_native_api::SettledDoorKey,
}

thread_local! {
    /// Per-thread memo of [`LfEntryRecord`]s indexed by the `vmentry` `MemberName`'s
    /// address, valid for one collection epoch under one VM (the argument of
    /// [`REAL_HANDLE_MEMO`]). A form that is recompiled gets a NEW `MemberName`
    /// (`vmentry` is assigned a fresh one), which misses.
    pub static LF_ENTRY_MEMO: std::cell::RefCell<crate::AddrMemo<LfEntryRecord>> =
        std::cell::RefCell::new(crate::AddrMemo::new(REAL_HANDLE_MEMO_SETS));
}

pub fn lf_entry_decode(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<LfEntryRecord> {
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

pub fn lf_entry_record(
    ctx: &mut dyn NativeContext,
    mn: ObjectRef,
) -> Option<std::rc::Rc<LfEntryRecord>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return lf_entry_decode(ctx, mn).map(std::rc::Rc::new);
    }
    let epoch = ctx.gc_address_epoch();
    let addr = mn.as_ptr() as usize;
    if let Some(hit) = LF_ENTRY_MEMO.with(|memo| memo.borrow_mut().get(vm, epoch, addr)) {
        return Some(hit);
    }
    let record = std::rc::Rc::new(lf_entry_decode(ctx, mn)?);
    LF_ENTRY_MEMO.with(|memo| memo.borrow_mut().insert(vm, epoch, addr, record.clone()));
    Some(record)
}

/// Is `obj` a `java.lang.Boolean` box? (See the `I` arm of
/// [`mh_real_lf_entry_fast`].)
pub fn lf_entry_is_boolean_box(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .is_some_and(|name| &*name == "java/lang/Boolean")
}

/// Run `this.form.vmentry(this, extra...)`: what `invokeBasic` is in HotSpot.
/// `None` declines (no entry yet, an arity this model does not line up): the
/// interpreter-based door runs instead, nothing disturbed. Arguments the JDK's
/// `NamedFunction`s pass boxed are unboxed against the entry's basic parameter
/// types; the result is the entry's own (raw for a primitive).
pub fn mh_real_lf_entry_fast(
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
            _ => *v,        };
        args.push(v);    }
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
pub fn mh_real_direct_dispatch(
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
    // (`L4W59HandleClassInitFrames` `mh-constructor`, `--real-jdk`). The
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
                    .flatten()                })            }
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
pub fn mh_carrier_leaf_desc(ctx: &mut dyn NativeContext, target: ObjectRef) -> String {
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
pub fn mh_linker_checks_receiver(
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
pub fn mh_real_lambda_form_dispatch(
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
pub fn real_static_accessor_target(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    offset: usize,
) -> Option<(ClassId, usize)> {
    let base = match ctx.get_field_by_name(this, "staticBase") {
        Value::Object(base) => base,
        _ => None,
    };
    crate::unsafe_natives_ext::unsafe_static_field_target_via_base(&*ctx, base, offset)}

/// The field kinds of [`mh_real_direct_dispatch`] (`REF_getField` ..
/// `REF_putStatic`): `member_type` is the field's `Class`.
pub fn mh_real_field_access(
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
pub fn mh_real_field_access_resolved(
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
                // `L3W54FieldOverrideAccessorFrames` under `--real-jdk`).
                note_field_handle_check_base_chain(ctx);
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
        }
    };
    // Interpreter round i2 wave 62, lane L3: the static field's class is
    // initialized first, as `DirectMethodHandle$StaticAccessor`'s barrier
    // does (`ensureInitialized`), listing its frames when the `<clinit>`
    // fails (`L4W59HandleClassInitFrames` `mh-static-getter` / `-setter`
    // under `--real-jdk`: the getter answered 0, the setter wrote). The
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
    // i2 wave 62, lane L4; `L4W62InheritedStaticFieldHandles`, `--real-jdk`).
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
pub fn mh_invoke_basic(
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
pub fn mh_invoke_door(
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
    if let Some(site) = call_site {
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
        call_site,
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
        adapter_entry_args(ctx, declared.as_deref(), extra, call_site)?
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
pub fn mh_invoke_null_receiver_after_casts_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Should the `invoke` door box a result against the handle's DECLARED return
/// rather than the call site's? Only for a reference call site over a
/// primitive declared return that differs from the leaf's primitive return
/// (the one shape where the call-site conversion has nothing to widen and
/// used to box the leaf's value). Kill switch
/// `CRATONVM_MH_INVOKE_BOXES_DECLARED_RETURN` (default on, both modes; round
/// 13 wave 3, lane ffm2).
pub fn invoke_boxes_declared_return(
    leaf_desc: &str,
    declared: Option<&str>,
    call_site: Option<&str>,
) -> bool {
    let Some(site) = call_site else {
        return false;
    };
    return_type_desc(site).starts_with('L') && boxes_declared_return(leaf_desc, declared)}

/// The `invokeWithArguments` door's half of [`invoke_boxes_declared_return`]:
/// its call site is always `Object`, so only the declared-vs-leaf test
/// remains. Round 13 wave 3 hand-back (lane ffm2): both overloads boxed a
/// primitive result against the LEAF's return (`auto_box_return(.., desc)`),
/// so `retLong.asType(()double).invokeWithArguments()` answered a `Long`
/// (and a cast chain's `(double)(int) r` came back as the integral box)
/// where HotSpot answers a `Double`.
pub fn invoke_with_arguments_boxes_declared_return(leaf_desc: &str, declared: Option<&str>) -> bool {
    boxes_declared_return(leaf_desc, declared)
}

/// A primitive declared return that differs from the leaf's primitive return,
/// with `CRATONVM_MH_INVOKE_BOXES_DECLARED_RETURN` on (default on, both
/// modes; round 13 wave 3, lane ffm2). The switch covers both the `invoke`
/// and the `invokeWithArguments` door.
pub fn boxes_declared_return(leaf_desc: &str, declared: Option<&str>) -> bool {
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
        true
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
pub fn mh_invoke_exact_door(
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
    if let Some(site) = call_site {
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
        plan.exact_tail(call_site)
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

