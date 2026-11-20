#[allow(unused_imports)]
use super::*;

/// `MethodHandle.invokeWithArguments(List)`'s door (see
/// [`mh_invoke_with_arguments_array_door`]).
pub fn mh_invoke_with_arguments_list_door(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // SBR5 lane avh: a REAL handle runs the JDK's own `asType(generic)` and
    // its spun entry (`mh_real_invoke_with_arguments_door`); a downcall
    // carrier stays on the door (`mh_with_arguments_runs_real`).
    if mh_with_arguments_runs_real(&*ctx, this) {
        if let Some(Value::Object(Some(list))) = args.get(1).copied() {
            let this_pin = ctx.pin_native_root(this);
            let list_pin = ctx.pin_native_root(list);
            let toarray = ctx.invoke_virtual(list, "toArray", "()[Ljava/lang/Object;", &[]);
            let this = ctx.read_native_pin(this_pin, this);
            let list = ctx.read_native_pin(list_pin, list);
            let answer = match toarray {
                // Interpreter round i4 wave 84 (lane L4): the JDK body
                // is `invokeWithArguments(arguments.toArray())`, a
                // VIRTUAL call, which a variable-arity handle answers
                // with `AsVarargsCollector`'s override (the jumbo path:
                // more than 127 trailing arguments collected by hand).
                // The door below is `MethodHandle`'s body, whose
                // `genericMethodType(n)` refuses n > 255 with
                // `IllegalArgumentException: bad parameter count n`:
                // `varargs.invokeWithArguments(listOf300)` failed where
                // HotSpot collects. `L4W84InvokeWithArgumentsJumbo`
                // rows `list-*`.
                Ok(Some(Value::Object(Some(arr))))
                    if mh_is_real_varargs_collector(&*ctx, this) =>
                {
                    ctx.invoke_virtual_bytecode_only(
                        this,
                        "invokeWithArguments",
                        "([Ljava/lang/Object;)Ljava/lang/Object;",
                        &[Value::Object(Some(arr))],
                    )
                }
                Ok(Some(Value::Object(Some(arr)))) => mh_real_invoke_with_arguments_door(
                    ctx,
                    this,
                    Some(arr),
                    Some((list_pin, list)),
                ),
                Ok(_) => mh_real_invoke_with_arguments_door(
                    ctx,
                    this,
                    None,
                    Some((list_pin, list)),
                ),
                Err(e) => Err(e),
            };
            // Releases `list_pin` too (pins are released from a handle on).
            ctx.unpin_native_roots(this_pin);
            return answer;
        }
    }
    // A real JDK `DelegatingMethodHandle` is its target (wave 45).
    let this = mh_door_handle(ctx, this);
    // Unpack the List via its own toArray() — layout-agnostic (the old
    // direct slot reads assumed the ArrayList layout and silently saw 0
    // args for any other List implementation).
    // NOTE: the cast refusal for this overload is applied after the
    // List is unpacked, below -- see the `Object[]` overload above for
    // why `invokeWithArguments` needs its own.
    //
    // gc-common w18-b: `List.toArray()` runs Java -- a GC point -- and
    // every later line uses `this` (the cast refusal, `MH_DESC`,
    // `MH_KIND`, the dispatch). Pin it across the call and re-read it,
    // the way `bindTo` below does; the `Object[]` overload above makes
    // no call before its uses and needs none.
    let this_pin = ctx.pin_native_root(this);
    let unpacked: Vec<Value> = match args.get(1) {
        Some(Value::Object(Some(l))) => {
            match ctx.invoke_virtual(*l, "toArray", "()[Ljava/lang/Object;", &[]) {
                Ok(Some(Value::Object(Some(arr)))) => {
                    let len = ctx.array_length(arr);
                    (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
                }
                Ok(_) => Vec::new(),
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
        }
        // The JDK body is `invokeWithArguments(arguments.toArray())`:
        // a null list is that call's helpful NPE (measured, HotSpot
        // 25). It used to invoke the handle with no arguments.
        // Interpreter round i1 wave 43, lane L4.
        Some(Value::Object(None)) => {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.util.List.toArray()\" because \"arguments\" \
                     is null"
                        .to_string(),
                ),
            }
            .into());
        }
        _ => Vec::new(),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(refusal) = invoke_with_arguments_count_refusal(ctx, this, unpacked.len()) {
        return Err(refusal);
    }
    // Read once, after the re-read (round 12 wave 7; see the
    // `Object[]` overload).
    let declared = mh_declared_descriptor(ctx, this);
    // Round 15 wave 4 (lane mh3): as the `Object[]` overload.
    let generic = generic_method_type_descriptor(unpacked.len());
    if let Some(refusal) =
        bound_handle_arity_refusal(ctx, this, &generic, declared.as_deref())
    {
        return Err(refusal);
    }
    // Wave 54 (lane L4): the kinds judged for `invokeExact` only.
    let kind_now = mh_kind_width_guarded(&*ctx, this);
    if let Some(refusal) = invoke_exact_typed_kind_arity_refusal(
        ctx,
        this,
        kind_now,
        &generic,
        declared.as_deref(),
    ) {
        return Err(refusal);
    }
    if let Some(refusal) =
        invoke_with_arguments_cast_refusal(ctx, this, &unpacked, declared.as_deref())
    {
        return Err(refusal);
    }
    // Same Object-return contract as the Object[] overload above: box
    // primitives, void → null.
    // JIT round 16 wave 11 (lane mhbind): `mh_downcall_door_type`.
    let (desc, kind) = mh_door_desc_and_kind(ctx, this);
    arm_entry_call_site(&generic);
    // Round 12 wave 6 (orchestrator, hunter4 page item 3): the reference
    // return cast `invoke` and `invokeExact` perform (`return_cast_refusal`,
    // `CRATONVM_MH_RETURN_CAST`). The declared descriptor is the one read
    // above (round 15 wave 5, lane mh4: it was rendered a second time
    // here, a mirror-to-name and a `String` per parameter per call; it
    // is a `String`, so nothing since can have changed it).
    let marks = mh_marking_bits(&*ctx, this);
    let explicit_stamp = marks & MH_MARK_EXPLICIT_CAST != 0;
    // Round 13 wave 3 hand-back (lane ffm2): a primitive declared
    // return the leaf does not share is boxed as the DECLARED type
    // (`invoke_with_arguments_boxes_declared_return`); that is work
    // after the dispatch, so the door is not transparent then.
    let boxes_declared =
        invoke_with_arguments_boxes_declared_return(&desc, declared.as_deref());
    // Wave 58 (lane L4): an `asType` copy's reference leaf return at
    // its primitive declared one; wave 59: one plan (`DoorReturn`).
    let plan = DoorReturn::with_arguments(
        kind,
        &desc,
        declared.as_deref(),
        marks,
        explicit_stamp,
        boxes_declared,
    );
    // The door boxes a primitive result and hands a reference back (`MH_TAIL`
    // token `L`, waves 26 and 28) unless the plan has work to do after
    // the dispatch (wave 60: the plan's own projection).
    let tail = if plan.with_arguments_works_after() {
        0
    } else {
        b'L'
    };
    let result = mh_dispatch_from_door(ctx, this, &unpacked, tail);
    // Wave 54 (lane L4): a leaf's arity refusal names this call site.
    let result = result.map_err(|failed| {
        mh_deep_refusal_site_text(ctx, failed, declared.as_deref(), Some(&generic), false)
    });
    door_return_apply(ctx, result, &plan)
}

pub fn register_t4_method_handle_invoke(r: &mut NativeMethodRegistry) {
            let mh = "java/lang/invoke/MethodHandle";

    // invoke(...) — polymorphic signature with automatic type adaptation.
    // Boxing/unboxing and widening conversions are applied implicitly.
    // Return value is auto-boxed since call-site expects Object.
    r.register(
        mh,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // TAKEN FIRST. `adapt_invoke_args` (in `mh_invoke_door`) unboxes
            // wrappers, which can re-enter the interpreter and reach the
            // signature-polymorphic dispatch block again — and that block
            // CLEARS the channel for every name it does not arm.
            let call_site = cratonvm_native_api::poly_call_site::take();
            // Interpreter round i5 wave 107, lane L4 (`CRATONVM_MH_ARMED_CALLER`):
            // the caller the arming door named with the site, read with it.
            let armed_caller = cratonvm_native_api::poly_call_site::taken_caller();
            // `CRATONVM_DBG=mh-finder-census` (wave 89, lane L4): this call's
            // clock, charged on every return.
            let _census_clock = mh_invoke_census_clock(&*ctx, this, false);
            // Interpreter round i4 wave 94, lane L4 (`CRATONVM_MH_FIELD_LINK_FIRST`,
            // default off): a linked synthetic field accessor at its declared
            // site, ahead of the real-handle lanes (see `mh_field_link_first`).
            if crate::nbflags().mh_field_link_first {
                if let Some(result) = mh_field_link_first(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    LinkingDoor::Invoke,
                ) {
                    return result;
                }
            }
            // Interpreter round i4 wave 88, lane L4
            // (`CRATONVM_MH_REAL_CTOR_WIDENING_SERVE`, default off): a real
            // nullary constructor handle at an `()Object` site.
            if let Some(result) =
                mh_real_ctor_widening_serve(ctx, this, &args[1..], call_site.as_deref())
            {
                return result;
            }
            // Interpreter round i4 wave 95, lane L4
            // (`CRATONVM_MH_REAL_FIELD_BEFORE_LINKER`, default off): a real
            // field accessor at a site of its own type, ahead of the linker.
            if let Some(result) =
                mh_real_field_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller)
            {
                return result;
            }
            // Interpreter round i5 wave 101, lane L4
            // (`CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER`, default off): a
            // proven `bindTo` of a real instance field accessor.
            if let Some(result) = mh_real_bound_field_before_linker(
                ctx,
                this,
                &args[1..],
                call_site.as_deref(),
                "invoke",
                armed_caller,
            ) {
                return result;
            }
            // Interpreter round i5 wave 102, lane L4
            // (`CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER`, default off): a real
            // static / virtual method handle at a site of its own type.
            if let Some(result) =
                mh_real_direct_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller)
            {
                return result;
            }
            if let Some(result) =
                mh_linked_invoker(ctx, this, &args[1..], call_site.as_deref(), "invoke")
            {
                return result;
            }
            // JIT round 17 wave 6 (lane mh6): the site's link refuses a class
            // the caller may not access, before any type comparison.
            // Interpreter round i4 wave 95, lane L4: and before the interface
            // null-receiver check below, which is the handle's own (its
            // `Interface.checkReceiver` runs inside the linked form), so
            // HotSpot's link answers first, as `mh_linked_invoker` orders them.
            if let Some(refusal) = mh_site_access_refusal(&*ctx, call_site.as_deref()) {
                return Err(refusal);
            }
            // JIT round 18 wave 11 (lane mhresid): an interface handle's null
            // receiver, with HotSpot's message.
            if let Some(npe) =
                mh_interface_null_receiver_npe(ctx, this, &args[1..], call_site.as_deref())
            {
                return Err(npe);
            }
            // SBR3 mhperf: a real `DirectMethodHandle` called at its own type.
            if let Some(result) =
                mh_real_direct_fast(ctx, this, &args[1..], call_site.as_deref(), false, true)
            {
                return result;
            }
            // A real JDK `DelegatingMethodHandle` is its target (wave 45).
            let this = mh_door_handle(ctx, this);
            // JIT round 17 wave 2 (lane mh3): equal descriptor strings naming
            // another loader's class are `asType`'s cast, not an exact call.
            if let Some(refusal) =
                mh_invoke_loader_cast_refusal(ctx, this, &args[1..], call_site.as_deref())
            {
                return Err(refusal);
            }
            // JIT round 17 wave 3 (lane mh4): the result's cast across
            // loaders, asked before the call (pure reads), applied after.
            // The array target's lookup can create the array class (no Java,
            // no collection): `this` stays valid.
            let ret_cast = mh_invoke_return_cast_target(ctx, this, call_site.as_deref());
            // Interpreter round i2 wave 67 (lane L4,
            // `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`): a linked field accessor
            // called at EXACTLY its declared type is `invokeExact`.
            if ret_cast.is_none() {
                // Wave 80 (`CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE`, default
                // off): a real getter at a converting site whose adapter the
                // handle's `asTypeCache` already holds.
                if let Some(result) =
                    mh_real_field_converting_serve(ctx, this, &args[1..], call_site.as_deref())
                {
                    return result;
                }
                if let Some(result) =
                    mh_field_link_fast_for_invoke(ctx, this, &args[1..], call_site.as_deref())
                {
                    return result;
                }
                // Wave 68 (`CRATONVM_MH_INVOKE_LINKS_FIELD`): not linked yet
                // (or relinking): link it here at the declared type, as
                // `invokeExact` would; the door serves this call -- or, wave
                // 69 (`CRATONVM_MH_LINKING_CALL_SERVED`), the link just written.
                if let Some(result) = mh_field_link_try_and_serve(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    LinkingDoor::Invoke,
                ) {
                    return result;
                }
            }
            let result = mh_invoke_registered_body(ctx, this, &args[1..], call_site.as_deref());
            match ret_cast {
                Some(target) => mh_invoke_return_cast(&*ctx, target, result),
                None => result,
            }
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // invokeExact(...) — strict type checking: argument count must match.
    // Throws WrongMethodTypeException if arity mismatches.
    // Return value is auto-boxed since call-site expects Object.
    r.register(
        mh,
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // TAKEN FIRST and unconditionally, for the reason `invoke` above
            // takes first: anything that re-enters the interpreter can reach
            // the signature-polymorphic dispatch block, which CLEARS the
            // channel for every name it does not arm. Taking it here also
            // guarantees this native never leaves a descriptor armed for a
            // later dispatch through a door that armed none.
            let call_site = cratonvm_native_api::poly_call_site::take();
            // Interpreter round i5 wave 107, lane L4 (`CRATONVM_MH_ARMED_CALLER`):
            // the caller the arming door named with the site, read with it.
            let armed_caller = cratonvm_native_api::poly_call_site::taken_caller();
            // `CRATONVM_DBG=mh-finder-census` (wave 89, lane L4).
            let _census_clock = mh_invoke_census_clock(&*ctx, this, true);
            // Interpreter round i4 wave 94, lane L4 (`CRATONVM_MH_FIELD_LINK_FIRST`,
            // default off): see `mh_field_link_first`.
            if crate::nbflags().mh_field_link_first {
                if let Some(result) = mh_field_link_first(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    LinkingDoor::InvokeExact,
                ) {
                    return result;
                }
            }
            // Wave 88, lane L3 (`CRATONVM_MH_REAL_CONSTRUCTOR_LANE`, default
            // off): the reflective accessor's nullary constructor handle at
            // `()Object`, ahead of the JDK invoker linkage below (which runs
            // the handle's whole `LambdaForm` chain on every call).
            if let Some(result) =
                mh_real_constructor_fast(ctx, this, &args[1..], call_site.as_deref())
            {
                return result;
            }
            // Interpreter round i9 (`CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE`,
            // default on, `--nojit` only): a typed one-argument constructor's
            // `asType` adapter (restored by t1 lane H; the lang_invoke
            // decomposition had dropped this call with the lane).
            if let Some(result) =
                mh_real_ctor_cast_adapter_fast(ctx, this, &args[1..], call_site.as_deref())
            {
                return result;
            }
            // Interpreter round i4 wave 95, lane L4
            // (`CRATONVM_MH_REAL_FIELD_BEFORE_LINKER`, default off).
            if let Some(result) =
                mh_real_field_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller)
            {
                return result;
            }
            // Interpreter round i5 wave 101, lane L4
            // (`CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER`, default off).
            if let Some(result) = mh_real_bound_field_before_linker(
                ctx,
                this,
                &args[1..],
                call_site.as_deref(),
                "invokeExact",
                armed_caller,
            ) {
                return result;
            }
            // Interpreter round i5 wave 102, lane L4
            // (`CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER`, default off): a real
            // static / virtual method handle at a site of its own type.
            set_mh_tail(call_site.as_deref().map_or(0u8, |cs| door_tail_token(return_type_desc(cs))));
            let before_linker =
                mh_real_direct_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller);
            set_mh_tail(0);
            if let Some(result) = before_linker {
                return result;
            }
            if let Some(result) =
                mh_linked_invoker(ctx, this, &args[1..], call_site.as_deref(), "invokeExact")
            {
                return result;
            }
            // Round 19: a downcall carrier the JIT would bind at this site
            // (none of the refusals below can fire for it).
            if let Some(result) =
                crate::panama::interp_downcall_lane(ctx, args, call_site.as_deref())
            {
                return result;
            }
            // JIT round 17 wave 6 (lane mh6): see `mh_site_access_refusal`.
            // Interpreter round i4 wave 95, lane L4: asked before the interface
            // null-receiver check (HotSpot links the site first), as
            // `mh_linked_invoker` and the `invoke` native order them.
            if let Some(refusal) = mh_site_access_refusal(&*ctx, call_site.as_deref()) {
                return Err(refusal);
            }
            // JIT round 18 wave 11 (lane mhresid): see `mh_interface_null_receiver_npe`.
            if let Some(npe) =
                mh_interface_null_receiver_npe(ctx, this, &args[1..], call_site.as_deref())
            {
                return Err(npe);
            }
            // SBR3 mhperf: a real `DirectMethodHandle` called at its own type.
            set_mh_tail(call_site.as_deref().map_or(0u8, |cs| door_tail_token(return_type_desc(cs))));
            if let Some(result) =
                mh_real_direct_fast(ctx, this, &args[1..], call_site.as_deref(), false, true)
            {
                return result;
            }
            set_mh_tail(0);
            // Wave 73 (`CRATONVM_MH_REAL_FIELD_RECORD`): a real field accessor
            // at its declared type, with the two site refusals below.
            if let Some(result) =
                mh_real_field_record_fast(ctx, this, &args[1..], call_site.as_deref(), false)
            {
                return result;
            }
            // A real JDK `DelegatingMethodHandle` is its target (wave 45).
            let this = mh_door_handle(ctx, this);
            // (The site's access refusal was asked above, wave 95.)
            if let Some(refusal) = mh_real_exact_refusal(ctx, this, call_site.as_deref()) {
                return Err(refusal);
            }
            if let Some(result) = mh_real_direct_dispatch(ctx, this, &args[1..], false) {
                return result;
            }
            if let Some(result) =
                mh_real_lambda_form_dispatch(ctx, this, &args[1..], call_site.as_deref(), false)
            {
                return result;
            }
            // Round 13 wave 10 (lane callcost4): see `mh_direct_lane`.
            if mh_direct_lane_enabled() {
                if let Some(result) = mh_direct_lane(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    DirectLaneDoor::InvokeExact,
                ) {
                    return result;
                }
            }
            // Round 16 (lane mh, MH16-2): an `asType` copy of a static handle
            // at its declared type takes the converting lane.
            if mh_converting_lane_enabled() {
                if let Some(result) = mh_converting_lane(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    DirectLaneDoor::InvokeExact,
                ) {
                    return result;
                }
            }
            // A field getter/setter linked on an earlier call is served
            // without the door; an unlinked one is linked when this call is
            // one the door serves by a plain field access (SBR wave 5, lane
            // reflacc -- see `mh_field_link_fast`).
            if let Some(result) = mh_field_link_fast(ctx, this, &args[1..], call_site.as_deref()) {
                return result;
            }
            // Wave 69 (`CRATONVM_MH_LINKING_CALL_SERVED`): the linking call
            // itself is served by the link it wrote.
            if let Some(result) = mh_field_link_try_and_serve(
                ctx,
                this,
                &args[1..],
                call_site.as_deref(),
                LinkingDoor::InvokeExact,
            ) {
                return result;
            }
            // Shared channel text (MH15-2, L7): the door borrows it.
            mh_invoke_exact_door(ctx, this, &args[1..], call_site.as_deref(), true)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mh,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        // Interpreter round i7 wave 138, lane L4: a named function, so the
        // interpreter can recognise the cached entry
        // ([`is_iwa_array_native_callback`]).
        mh_iwa_array_intrinsic,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        mh,
        "invokeWithArguments",
        "(Ljava/util/List;)Ljava/lang/Object;",
        // Wave 104, lane L3: as the array overload.
        |ctx, args| {
            // Wave 109, lane L4 (`CRATONVM_MH_IWA_JDK_BODY`, default off).
            if let Some(answer) =
                mh_iwa_jdk_body(ctx, args, "(Ljava/util/List;)Ljava/lang/Object;")
            {
                return answer;
            }
            with_invoke_with_arguments_frames(
                ctx,
                args,
                cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArgumentsList,
                mh_invoke_with_arguments_list_door,
            )
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        mh,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let recv = args.get(1).copied().unwrap_or(Value::Object(None));
            // SBR5 lane avh: a repeat bind of a REAL handle is the JDK's answer for the
            // first bind with the bound argument replaced (`mh_real_bind_to`).
            if let Some(answer) = mh_real_bind_to(ctx, this, recv) {
                return answer;
            }
            // Wave 54 (lane L4): a real JDK handle binds through the JDK.
            if let Some(answer) = mh_foreign_bind_to(ctx, this, recv) {
                return answer;
            }
            // Interpreter round i2 wave 62, lane L4 (`CRATONVM_MH_INSTANCE_OWNER_ID`):
            // an unbound instance handle's recorded owner casts the receiver by
            // id, before anything here allocates. A lambda proxy's handle
            // (`MH_CLASS` `""`, a `type` naming no class) bound any value.
            if let Some(refusal) = bind_to_owner_refusal(ctx, this, recv) {
                return Err(refusal);
            }
            // gen r4w3/rooting: `this` and `recv` are read and stored after
            // `alloc_method_handle` / `build_method_type_from_descriptor`
            // (GC points) below. Pin both now; `this_pin` (lowest) releases
            // everything pinned in this native before it returns a handle.
            let this_pin = ctx.pin_native_root(this);
            let recv_pin = pin_mh_value(ctx, recv);
            // JIT round 16 wave 11 (lane mhbind; MHF-2,
            // `CRATONVM_MH_BINDTO_FAST_MINT`): a repeat bind of the same,
            // unchanged source in this collection epoch, with a bound value
            // the line admits, is the mint alone (`bindto_fast_mint_hit`).
            // The key is read before anything here can collect.
            let mint_key = bindto_fast_mint_probe(&*ctx, this);
            if let Some(key) = &mint_key {
                if let Some(mut answer) = bindto_fast_mint_hit(&*ctx, key, recv) {
                    // Wave 66: a field line's carry, re-read off the source.
                    if bindto_fast_mint_refresh_carry(&*ctx, this, &mut answer) {
                        return bindto_fast_mint_finish(
                            ctx, this_pin, this, recv_pin, recv, &answer,
                        );
                    }
                }
            }
            // What the cast verdict below lets the fast mint remember.
            let mut mint_admits: Option<Option<u32>> = None;
            // W7-19: `bindTo` must REFUSE a target with no leading reference
            // parameter. `MethodHandle.bindTo`'s javadoc: "@throws
            // IllegalArgumentException if the target does not have a leading
            // parameter type that is a reference type", implemented in
            // `MethodType.leadingReferenceParameter()` as
            //   if (ptypes.length == 0 || ptypes[0].isPrimitive())
            //       throw newIllegalArgumentException("no leading reference parameter");
            // — so the test is arity-and-primitiveness, nothing else, and the
            // message is verbatim. Measured on the shipped binary under
            // `--fast-boot`: `findStatic(…(String,int)int).bindTo("abc")
            // .bindTo(2)` was ACCEPTED and answered 6, and `(int)int`
            // .bindTo(3) was accepted too; HotSpot 25 raises
            // `IllegalArgumentException: no leading reference parameter` for
            // both. A missing refusal, not a wrong value.
            //
            // The parameter list is read from the `type` field ONLY, never
            // from `mh_type_descriptor`'s `MH_DESC` fallback. That distinction
            // is the whole safety of this check: `MH_DESC` on a
            // virtual/special handle omits the receiver `alloc_method_handle`
            // prepends to `type`, so `Holder.pub`'s `(I)I` would read as a
            // primitive leading parameter and this would refuse a bind that
            // HotSpot accepts. No `type` MethodType, or one whose mirrors do
            // not render, means the leading parameter is UNKNOWN and the bind
            // is allowed through — a refusal is only ever raised on a positive
            // reading.
            //
            // DONE 2026-08-30, narrowly — the `cast` half of the JDK's one-line
            // body `type.leadingReferenceParameter().cast(x)`, which also raises
            // `ClassCastException` for a wrong REFERENCE type. See the second
            // check below.
            //
            // W7-19 §5.1 deferred it, and its reason still governs the SHAPE of
            // the check: an assignability question asked with `is_subclass`
            // alone answers FALSE for a fabricated stand-in against a real JDK
            // interface (a fabricated class declares no interfaces, so every
            // type test against one fails), and `bindTo` is on the Groovy-indy /
            // SpEL-FunctionReference / log4j-provider path in this tree, all of
            // which bind interfaces and subtypes. A false `ClassCastException`
            // there refuses working code, which is worse than the wrong answer
            // it replaces.
            //
            // What changed is not the risk but the available predicates: the
            // check below never refuses an INTERFACE target (where the false
            // negative lives), consults `synthetic_implements_declared` for the
            // VM-minted stand-ins, and refuses only on a positive reading. The
            // lane §5.1 asked for also exists now —
            // `probes/CodegenFrameworkSmoke.java` boots Groovy, ByteBuddy,
            // Mockito, ASM, Javassist and Objenesis as themselves.
            //
            // The test immediately below stays syntactic — is the first
            // descriptor token `L…;`/`[…` — and cannot be wrong for a reason
            // outside its own two lines.
            //
            // Round 16 (lane mh): the render is kept (`rendered_type`) for the
            // bound type below (`bindto_bound_type_descriptor_with`).
            let mut rendered_type: Option<String> = None;
            if let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") {
                if let Some(tdesc) = methodtype_to_descriptor(ctx, mt) {
                    if let Some((params, _)) = split_descriptor_params(&tdesc) {
                        let leading_is_reference = params
                            .first()
                            .is_some_and(|p| p.starts_with('L') || p.starts_with('['));
                        if !leading_is_reference {
                            return Err(RuntimeError::IllegalArgumentException {
                                message: "no leading reference parameter".to_string(),
                            }
                            .into());
                        }
                        // The `cast` half of `type.leadingReferenceParameter()
                        // .cast(x)`, deferred above until it could be written
                        // WITHOUT the false positive that made it dangerous.
                        //
                        // Measured, `probes/L8InvokeLookupSweep.java`:
                        //   findVirtual(String, "length", ()int)
                        //     .bindTo(Integer.valueOf(1))
                        //   HotSpot   ClassCastException
                        //   CratonVM  accepted, answers a handle of type ()int
                        // A missing check, and the wrong receiver is then live
                        // in a handle that looks perfectly well-typed.
                        //
                        // `reference_arg_admitted` is the shared predicate --
                        // `MethodHandle.invoke` needs the identical judgement on
                        // its own arguments, and writing it twice is how the two
                        // doors come to disagree about one object. It answers
                        // `Some(false)` only on a positive mismatch; every
                        // "cannot tell" case is an allow, and its doc comment
                        // has the reason for each.
                        if let Some(pdesc) = params.first().cloned() {
                            let verdict = reference_arg_admitted(ctx, recv, &pdesc);
                            if verdict == Some(false) {
                                return Err(reference_cast_failure(ctx, recv, &pdesc));
                            }
                            if mint_key.is_some() {
                                mint_admits = bindto_fast_mint_admits(&*ctx, recv, &pdesc, verdict);
                            }
                        }
                    }
                    rendered_type = Some(tdesc);
                }
            }
            // Clone the MH and set BOUND field
            let class = mh_read_class(ctx, this);
            let name = mh_read_name(ctx, this);
            let desc = mh_read_desc(ctx, this);
            // Round 16 (lane mh): whether all three decoded, for
            // `bindto_shared_mint_applies_read` (which used to decode them again).
            let strings_read = class.is_some() && name.is_some() && desc.is_some();
            let class = class.unwrap_or_default();
            let name = name.unwrap_or_default();
            let desc = desc.unwrap_or_default();
            let kind = match ctx.get_field(this, MH_KIND) {
                Value::Int(k) => k,
                _ => MH_KIND_VIRTUAL,
            };
            // A SECOND `bindTo` on an already-bound direct (static/virtual/special)
            // handle binds the NEXT parameter, not the one already captured in the
            // single-slot MH_BOUND. Overwriting MH_BOUND silently dropped the first
            // capture — e.g. `String::formatted`.bindTo(template).bindTo(argsArray)
            // lost the `template` receiver, so dispatch invoked `formatted` on the
            // bound `Object[]` and hard-failed `NoSuchMethodError Object.formatted`
            // (ExpressionLanguageScenarioTests #messageBound / #messageStaticBound).
            // Model the extra capture as `insertArguments(this, 0, {recv})`: the
            // INSERT adapter splices `recv` into `this`'s remaining leading
            // parameter at dispatch while `this` keeps its own prior binding.
            //
            // Round 15 wave 4 (lane mh3): and so does every kind whose dispatch
            // does not read `MH_BOUND` as the captured leading argument (an
            // adapter's wrapper, a constructor's class id, an invoker, a static
            // setter; see `bindto_takes_insert_adapter`). A constructor's
            // `MH_DESC` returns `V`, which the doors box to `null`, so its
            // adapter carries the constructor's `type` descriptor instead
            // (read here, while `this` is current).
            let already_bound = matches!(ctx.get_field(this, MH_BOUND), Value::Object(Some(_)));
            // Round 15 wave 5 (lane mh4): an unbound static handle's recorded
            // owner (`MH_BOUND`'s `Int`), which the bound copy keeps in
            // `MH_STATIC_OWNER`. Read while `this` is current; an `Int`.
            let static_owner = if kind == MH_KIND_STATIC && !already_bound {
                mh_static_owner(ctx.get_field(this, MH_BOUND))
            } else if kind == MH_KIND_SPECIAL && !already_bound {
                // Interpreter round i2 wave 64, lane L4: a `findSpecial`
                // handle's dispatch class by id (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`)
                // rides to the bound copy in the same slot, so the bound
                // handle's `SPECIAL` arm calls by it too.
                mh_special_owner_with(&*ctx, this, mh_special_by_owner_id_enabled())
            } else {
                None
            };
            // Interpreter round i2 wave 65, lane L4 (`CRATONVM_MH_BOUND_FIELD_OWNER`):
            // an unbound instance field handle's recorded owner and minted
            // slot ride to the bound copy, so its arm resolves the slot by id.
            // Read while `this` is current.
            let field_carry = if kind == MH_KIND_GETTER || kind == MH_KIND_SETTER {
                bound_field_carry_with(
                    &*ctx,
                    this,
                    kind,
                    &desc,
                    mh_instance_owner(&*ctx, this),
                    mh_bound_field_owner_enabled(),
                )
            } else {
                None
            };
            // A Panama downcall carrier has no `MH_KIND` (read as virtual
            // above) and dispatches through `mh_dispatch`'s downcall arm, so
            // `downcall.bindTo(allocator)` takes the adapter too; the copy
            // minted a virtual handle with no class, name or descriptor,
            // which is no longer a downcall and cannot dispatch.
            let insert_adapter =
                ((kind == MH_KIND_STATIC || kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL)
                    && already_bound)
                    || bindto_takes_insert_adapter(kind, already_bound, &desc)
                    || bindto_null_takes_insert_adapter(kind, recv, &desc)
                    || (crate::panama::is_downcall_handle(&*ctx, this));
            let constructor_type = if insert_adapter && kind == MH_KIND_CONSTRUCTOR {
                mh_type_descriptor(ctx, this)
            } else {
                None
            };
            if insert_adapter {
                // gc-common w20-f: `recv` and `this` are re-read AFTER the
                // array's allocation (they were re-read before it and stored
                // after it), and the carrier holds `this` and the array at
                // their current addresses.
                let values = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
                let recv = read_pinned_mh_arg(ctx, recv_pin, recv);
                ctx.set_array_element(values, 0, recv);
                let this = ctx.read_native_pin(this_pin, this);
                let wrapper = alloc_mh_carrier_holding(
                    ctx,
                    "__mh_insert_wrapper__",
                    &[
                        Value::Object(Some(this)),
                        Value::Object(Some(values)),
                        Value::Int(0),
                    ],
                );
                // gen r4w3/rooting: pin `wrapper` across the allocation;
                // re-read `this`, and keep `adapter` pinned across the
                // MethodType build.
                let wrapper_pin = ctx.pin_native_root(wrapper);
                let adapter = alloc_method_handle(
                    ctx,
                    "__adapter__",
                    "insert",
                    constructor_type.as_deref().unwrap_or(&desc),
                    MH_KIND_INSERT,
                )?;
                let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
                let this = ctx.read_native_pin(this_pin, this);
                let adapter_pin = ctx.pin_native_root(adapter);
                ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
                // Binding one value REMOVES the leading parameter from `this`'s
                // type. The adapter's `type()` must reflect that (e.g. a fully
                // bound handle reports arity 0); otherwise SpEL's
                // `FunctionReference` reads a stale 1-arg type and re-wraps the
                // call args via `setupArgumentsForVarargsInvocation`, nesting an
                // extra empty `Object[]` into the varargs
                // (#messageBound → "...[Ljava.lang.Object;@..."). Mirror the
                // `MethodHandles.insertArguments` type adjustment.
                if let Some(tdesc) = mh_type_descriptor(ctx, this) {
                    if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                        if !params.is_empty() {
                            params.remove(0); // one value inserted at pos 0
                        }
                        let new_desc = format!("({}){}", params.concat(), ret);
                        let src = Some((ctx.read_native_pin(this_pin, this), kind));
                        if let Ok(Some(mt)) = bound_method_type_from_descriptor(ctx, &new_desc, src)
                        {
                            let adapter = ctx.read_native_pin(adapter_pin, adapter);
                            ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
                        }
                    }
                }
                let adapter = ctx.read_native_pin(adapter_pin, adapter);
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(adapter))));
            }
            // Round 12 wave 7: what a stamped `this` (an `asType` /
            // `explicitCastArguments` copy) owes the value it binds and the
            // handle this mints -- the casts of the types beneath its `type`
            // (`bound_cast_chain`; `bindTo`'s own cast above is to the outer
            // type only), its explicit marking, and for a virtual/special
            // handle its own `type` minus the receiver rather than `MH_DESC`.
            // A fresh mint from `MH_CLASS`/`MH_DESC` dropped all three:
            // `findVirtual(Mid, m).asType((Object,int)int).bindTo(grand)`
            // dispatched `Mid.m` on a `Grand`. Read here, before the
            // allocation below, while `this` and `recv` are current.
            let bound_chain = bound_cast_chain(ctx, this, recv, kind);
            let chain_free = bound_chain.is_none();
            let bound_explicit = bound_chain.is_some() && mh_is_explicit_cast(&*ctx, this);
            let bound_virtual_type = if bound_chain.is_some()
                && (kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL)
            {
                mh_declared_descriptor(ctx, this).and_then(|d| descriptor_without_first_param(&d))
            } else {
                None
            };
            // Round 15 wave 4 (lane mh3): the bound handle is FIXED-arity
            // (`mh_bindto_fixed_arity_enabled`), and, for the kinds
            // `bindto_shared_mint_applies` names, minted with `this`'s own
            // strings and its bound `type` built once (MH2-3). `shared` says
            // the two `type` rebuilds below have nothing left to do.
            let fixed_arity = mh_bindto_fixed_arity_enabled();
            let shared = bindto_shared_mint_applies_read(&*ctx, this, kind, strings_read);
            // MHF-2: the shared mint's two answers, for the fast-mint line.
            let mut mint_answer: Option<(std::rc::Rc<str>, bool)> = None;
            let new_mh = if shared {
                let type_desc = bindto_bound_type_descriptor_with(
                    ctx,
                    this,
                    kind,
                    &desc,
                    bound_virtual_type.as_deref(),
                    rendered_type.as_deref(),
                );
                let collects =
                    !fixed_arity && method_is_variable_arity(&*ctx, &class, &name, &desc, kind);
                if mint_key.is_some() && fixed_arity {
                    mint_answer = Some((std::rc::Rc::from(type_desc.as_str()), collects));
                }
                // Wave 66 (`CRATONVM_MH_FIELD_LINK_BOUND`): wide for the bound
                // field link too.
                let link_slots =
                    bound_field_copy_needs_link_slots(field_carry, mh_field_link_bound_enabled());
                alloc_bound_method_handle(
                    ctx, this_pin, this, kind, &type_desc, collects, link_slots,
                )?
            } else {
                let new_mh = alloc_method_handle(ctx, &class, &name, &desc, kind)?;
                if fixed_arity {
                    mh_set_varargs_collector(&*ctx, new_mh, false);
                }
                new_mh
            };
            // gen r4w3/rooting: re-read `this` after the allocation, and keep
            // `new_mh` pinned across the MethodType builds below.
            let this = ctx.read_native_pin(this_pin, this);
            let new_mh_pin = ctx.pin_native_root(new_mh);
            // `bindTo` captures the LEADING argument, so the bound handle's
            // `type()` must have that leading parameter REMOVED (HotSpot:
            // `(Recv,Object)int`.bindTo(r) -> `(Object)int`). For virtual/special
            // handles the leading param is the receiver that `alloc_method_handle`
            // just prepended to `type`; reset `type` to the raw (receiver-less)
            // descriptor so e.g. Groovy's indy `sameClasses` guard builds a
            // classes[] of the right length (was AIOOBE: classes longer than the
            // runtime args). STATIC-handle leading-arg drop is a separate path.
            if shared {
                // Built by `alloc_bound_method_handle` from the same answers
                // (`bindto_bound_type_descriptor`).
            } else if kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL {
                let type_desc = bound_virtual_type.as_deref().unwrap_or(&desc);
                let src = Some((ctx.read_native_pin(this_pin, this), kind));
                if let Ok(Some(mt)) = bound_method_type_from_descriptor(ctx, type_desc, src) {
                    let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
                    ctx.set_field_by_name(new_mh, "type", Value::Object(Some(mt)));
                }
            } else if kind == MH_KIND_STATIC
                || kind == MH_KIND_GETTER
                || kind == MH_KIND_SETTER
                || kind == MH_KIND_ARRAY_GET
                || kind == MH_KIND_ARRAY_SET
                || kind == MH_KIND_IDENTITY
                || kind == MH_KIND_CONSTANT
                || kind == MH_KIND_THROW
                || kind == MH_KIND_ARRAY_LENGTH
                // Round 15 wave 4 (lane mh3): a lambda factory's `type` kept
                // every capture, so a SECOND bind (log4j's `factory.bindTo(
                // serviceType).bindTo(classLoader)`) was cast by the guard above
                // against the FIRST capture's type and refused. Its dispatch
                // counts captures from `MH_DESC`, never from `type`.
                || (kind == MH_KIND_LAMBDA_FACTORY)
            {
                // Wave 40 (lane L4) added `identity` and `constant`/`empty`:
                // `identity(String).bindTo(x)` is `()String` and
                // `empty((String)int).bindTo(x)` is `()int` on HotSpot; the
                // bound copy kept `(String)String` / `(String)int`, which
                // `invokeExact`'s type check (now judging both kinds) would
                // have refused at the `()String` site, and a second `bindTo`
                // was accepted where HotSpot refuses it.
                // `arrayLength(int[].class).bindTo(a)` likewise is `()int`.
                // STATIC `bindTo` captures the leading PARAMETER (not a receiver),
                // so the bound handle's `type()` must drop that leading parameter
                // — `(String,String[])R`.bindTo(s) -> `(String[])R`. The dispatch
                // path keys off MH_DESC, but downstream `type()` readers do not:
                // SpEL's `FunctionReference` reads the arity to decide varargs
                // repackaging, and a chained `bindTo` (#messageStaticBound) derives
                // its own arity from this one. Leaving the full type here left a
                // stale extra parameter that mis-packed the varargs.
                //
                // W7-19 §5.3 added the four accessor kinds, which take the same
                // drop for the same reason and were the only kinds left out.
                // Measured on the shipped binary against HotSpot 25, same class
                // file: `findGetter(H,"i",int)` bound to a receiver reported
                // `(H)int` where HotSpot reports `()int`, and
                // `arrayElementGetter(int[])` bound to an array reported
                // `([I,int)int` where HotSpot reports `(int)int`. A getter's
                // `type` is its raw descriptor `(LH;)I` — `alloc_method_handle`
                // prepends a receiver only for VIRTUAL/SPECIAL — so slot 0 of the
                // parameter list IS the value `bindTo` just captured, exactly as
                // for STATIC. Static getters/setters (`()I`, `(I)V`) never reach
                // here: the guard at the top of this native already refuses a
                // zero-arity or primitive-leading target, which is what HotSpot
                // does too.
                //
                // This TIGHTENS the guard above on those two shapes, and that is
                // the point rather than a side effect: those were the only two
                // rows in W7-19 §3.3's thirteen-shape census where CratonVM
                // under-refused a second `bindTo` that HotSpot refuses. Before
                // this, a second bind on a bound getter fell through to the
                // allocation below and silently OVERWROTE the first capture;
                // now it raises HotSpot's `IllegalArgumentException`.
                if let Some(tdesc) = mh_type_descriptor(ctx, this) {
                    if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                        if !params.is_empty() {
                            params.remove(0);
                        }
                        let new_desc = format!("({}){}", params.concat(), ret);
                        let src = Some((ctx.read_native_pin(this_pin, this), kind));
                        if let Ok(Some(mt)) = bound_method_type_from_descriptor(ctx, &new_desc, src)
                        {
                            let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
                            ctx.set_field_by_name(new_mh, "type", Value::Object(Some(mt)));
                        }
                    }
                }
            }
            // gen r4w3/rooting: current addresses after the MethodType builds.
            let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
            let this = ctx.read_native_pin(this_pin, this);
            let recv = read_pinned_mh_arg(ctx, recv_pin, recv);
            if kind == MH_KIND_LAMBDA_FACTORY {
                // A lambda factory may capture more than one value (e.g. log4j's
                // `factory.bindTo(serviceType).bindTo(classLoader)`). The single-slot
                // MH_BOUND used by the other kinds would drop all but the last bind,
                // so accumulate captures, in bind order, into an Object[].
                //
                // gc-common w15-e: the capture array is allocated AFTER the
                // previous captures, `recv` and `new_mh` were read, and all
                // three are used after it. The pins used to be released
                // before that allocation; now `this_pin` (and with it
                // `recv_pin` / `new_mh_pin`) is held across it, the previous
                // capture array gets its own pin, and each is re-read after.
                let prev = ctx.get_field(this, MH_BOUND);
                let old_pin = match prev {
                    Value::Object(Some(old)) => Some((ctx.pin_native_root(old), old)),
                    _ => None,
                };
                let oldlen = match old_pin {
                    Some((_, old)) => ctx.array_length(old),
                    None => 0,
                };
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, oldlen + 1);
                if let Some((pin, old)) = old_pin {
                    let old = ctx.read_native_pin(pin, old);
                    for i in 0..oldlen {
                        let v = ctx.get_array_element(old, i);
                        ctx.set_array_element(arr, i, v);
                    }
                }
                let recv = read_pinned_mh_arg(ctx, recv_pin, recv);
                ctx.set_array_element(arr, oldlen, recv);
                let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
                ctx.set_field(new_mh, MH_BOUND, Value::Object(Some(arr)));
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(new_mh))));
            }
            // Round 12 wave 7: the stamped handle's chain, minus the bound
            // parameter, and its explicit marking (see `bound_chain` above).
            // The chain's text is allocated: `new_mh` comes back current, and
            // `recv` is re-read from its pin before the pins are released.
            let (new_mh, recv) = match bound_chain {
                Some(chain) => {
                    if bound_explicit {
                        mh_mark_explicit_cast(ctx, new_mh);
                    }
                    let new_mh = mh_store_cast_chain(ctx, new_mh, &chain);
                    (new_mh, read_pinned_mh_arg(ctx, recv_pin, recv))
                }
                None => (new_mh, recv),
            };
            // A `constant` / `empty` handle keeps answering its constant: the
            // bound argument is dropped, exactly as its dispatch ignores every
            // argument (wave 40, lane L4). Storing `recv` here made
            // `empty((String)int).bindTo("x")` unbox the String as its `int`
            // answer. No allocation since `this` was re-read (`bound_cast_chain`
            // is `None` for this kind), so the read through the pin is current.
            let recv = if kind == MH_KIND_CONSTANT {
                let this = ctx.read_native_pin(this_pin, this);
                ctx.get_field(this, MH_BOUND)
            } else {
                recv
            };
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(new_mh, MH_BOUND, recv);
            if let Some(owner) = static_owner {
                if ctx.object_num_fields(new_mh) > MH_STATIC_OWNER
                    && mh_bindto_static_owner_enabled()
                {
                    ctx.set_field(new_mh, MH_STATIC_OWNER, Value::Int(owner.as_u32() as i32));
                }
            }
            // Wave 65 (lane L4): the field carry (`field_carry` above). No GC
            // point since `new_mh` was last made current.
            let mut prelink = None;
            if let Some(carry) = field_carry {
                mh_write_bound_field_carry(&*ctx, new_mh, carry);
                // Wave 67 (`CRATONVM_MH_BINDTO_PRELINK_FIELD`): link the copy
                // now, so its first `invokeExact` is served by the link. After
                // every write of the mint (`type`, the chain, `MH_BOUND`).
                mh_bindto_prelink_bound_field(ctx, new_mh);
                // Wave 68 (`CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`): what the
                // line below may remember of that link.
                prelink = bindto_prelink_memo_of(&*ctx, new_mh, carry);
            }
            // MHF-2: this ordinary mint's answers, for the next bind of the
            // same source (`bindto_fast_mint_fill` drops them if a collection
            // ran since `mint_key` was read). Only the unbound direct kinds,
            // with nothing chained: their slow path is the tail above.
            if let (Some(key), Some(recv_class), Some((type_desc, collects))) =
                (mint_key, mint_admits, mint_answer)
            {
                if chain_free
                    && !already_bound
                    && bindto_fast_mint_fills_kind(kind, mh_bindto_fast_mint_fields_enabled())
                {
                    let answer = BindToMintAnswer {
                        kind,
                        type_desc,
                        collects,
                        static_owner: static_owner.map(|o| o.as_u32()),
                        recv_class,
                        field_carry,
                        prelink,
                        alt: None,
                    };
                    bindto_fast_mint_fill(&*ctx, key, answer);
                }
            }
            Ok(Some(Value::Object(Some(new_mh))))
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    // asTypeUncached — the JDK's own body, except for a downcall carrier
    // (`mh_as_type_uncached_carrier`). A reviewed `Intrinsic`: it must win over
    // the real bytecode in both modes, and for every other receiver it IS that
    // bytecode.
    r.register(
        mh,
        "asTypeUncached",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let new_type = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Some(answer) = mh_as_type_uncached_carrier(ctx, this, new_type) {
                return answer;
            }
            // Interpreter round i3 wave 82, lane L4: the converting adapter
            // memo (`CRATONVM_MH_REAL_AS_TYPE_MEMO`, default off; `None`
            // unless it applies, then the body below runs as before).
            if let Some(answer) = mh_real_as_type_memo(ctx, this, new_type) {
                return answer;
            }
            ctx.invoke_virtual_bytecode_only(
                this,
                "asTypeUncached",
                "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
                &[new_type],
            )
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    // asType — type adaptation: return self, but propagate the supplied
    // MethodType into the `type` field so subsequent JDK-internal reads of
    // `mh.type()` / `parameterSlotCount` reflect the adapted signature.
    // invoke()/invokeExact handle the actual argument coercions.
    //
    // G31: and REFUSE the conversions HotSpot refuses, which this body did not
    // do at all. See [`mh_can_convert`] for the transcribed rule and the sweep
    // it was checked against. Two things about the shape of the refusal here:
    //
    // **It is gated on both descriptors, and on a REAL `MethodType` receiver.**
    // The check only runs when the handle's `type` field holds a MethodType
    // this file can turn back into a descriptor and the requested MethodType
    // does too. A handle whose type is the `MH_DESC` fallback is left alone:
    // that descriptor is the raw bytecode signature, not the adapted one, and
    // refusing on it would refuse conversions HotSpot allows.
    //
    // **It never refuses where the divergence is OURS.** HotSpot's `asType`
    // returns a NEW handle and leaves the receiver's `type()` untouched
    // (MEASURED: `identity(int).asType((int)long)` answers `(int)long` while
    // the receiver still answers `(int)int`, and the two are different
    // objects). This body has always had ONE object, so a second `asType` on
    // the same reference sees the FIRST one's adapted type where HotSpot would
    // still see the original. Adding a check on top of that aliasing would
    // manufacture refusals HotSpot never issues, so a conversion that the raw
    // `MH_DESC` signature would have allowed is accepted even when the mutated
    // `type` field forbids it. That is a deliberate UNDER-refusal in exactly
    // the cases the aliasing creates, and it cannot turn an accept into a
    // refusal. Collapsing the aliasing (minting a real second handle) is
    // NOMINATED in G31-1, not done here: it would have to reproduce every
    // synthetic dispatch slot of an arbitrary handle kind, and this lane could
    // not build the VM to find out what that breaks.
    // Reviewed intrinsic: it handles this VM's FFM downcall carrier copy,
    // while `mh_foreign_as_type` delegates every real JDK handle to its
    // concrete `MethodHandle.asType` bytecode. Without this resolution entry,
    // Real-JDK runs bytecode against the VM-minted abstract MethodHandle
    // carrier and reaches `rebind().editor()` (which only BoundMethodHandle
    // implements).
    r.register(
        mh,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // A Panama downcall's dispatch reads its `FunctionDescriptor`, not
            // its `type` field, so `asType` stays a passthrough for one.
            //
            // The refusal this guard used to be written against is gone: it
            // tested for the invented class `java/lang/foreign/DowncallHandle`
            // and existed because assigning `type` would have overwritten the
            // function address that class kept in field 0. The carrier is now a
            // real `MethodHandle` whose slot 0 IS the real `type` field, so
            // that write would be correct rather than destructive — but the
            // passthrough is still right, and cheaper.
            //
            // Round 15 wave 6 (lane mh5): not for `asType(null)`, which is the
            // NPE below on every handle (the passthrough answered the handle).
            //
            // JIT round 16 wave 11 (lane mhbind): the passthrough answered the
            // receiver for ANY new type, so `abs.asType((int)long)
            // .invokeExact(-6)` was refused with `WrongMethodTypeException`
            // where HotSpot answers 6. With `CRATONVM_MH_DOWNCALL_INVOKE_ADAPT`
            // (default on) a downcall answers an adapted copy
            // (`mh_downcall_as_type`); `0` restores the passthrough.
            if let Some(Value::Object(Some(this))) = args.first() {
                if crate::panama::is_downcall_handle(ctx, *this)
                    && !matches!(args.get(1), Some(Value::Object(None)))
                {
                    if mh_downcall_adapt_enabled() {
                        if let Some(Value::Object(Some(mt))) = args.get(1) {
                            return mh_downcall_as_type(ctx, *this, *mt);
                        }
                    }
                    return Ok(Some(args[0]));
                }
            }
            // `asType(null)`. MEASURED on HotSpot 25.0.3+9, transcribed rather
            // than composed — it is the helpful-NPE text for the first field
            // read `asTypeUncached` performs on `newType`:
            //   java.lang.NullPointerException: Cannot invoke
            //   "java.lang.invoke.MethodType.form()" because "newType" is null
            // This body used to hand the receiver straight back for a null
            // argument, which is the silent-lie shape: a caller asking for an
            // adaptation it did not describe got an unadapted handle.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException {
                    message: Some(
                        "Cannot invoke \"java.lang.invoke.MethodType.form()\" because \
                         \"newType\" is null"
                            .to_string(),
                    ),
                }
                .into());
            }
            if let (Some(Value::Object(Some(this))), Some(Value::Object(Some(mt)))) =
                (args.first(), args.get(1))
            {
                let (this, mt) = (*this, *mt);
                // Wave 54 (lane L4): a real JDK handle adapts through the
                // JDK's own `asType`; this model would stamp it in place.
                if let Some(answer) = mh_foreign_as_type(ctx, this, mt) {
                    return answer;
                }
                // Round 15 wave 6 (lane mh5, MH4-1): a variable-arity
                // receiver answers the JDK's FIXED-arity composition.
                if let Some(answer) = mh_astype_varargs_collector(ctx, this, mt) {
                    return answer;
                }
                if let Some(refusal) = mh_astype_refusal(ctx, this, mt) {
                    return Err(refusal);
                }
                return mh_with_stamped_type(ctx, Value::Object(Some(this)), mt, false);
            }
            Ok(Some(args[0]))
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );

    // C21: rebind() is JDK-abstract (no Code attribute) — synthetic MHs that
    // do not subclass BoundMethodHandle still get rebind() called by JDK
    // internals (e.g. Invokers, LambdaForm specialization). Return self so the
    // chain continues without "no Code attribute" internal errors.
    //
    // DO NOT RETIRE. Confirmed against all nine supported images: `rebind()` is
    // `abstract` on `java.lang.invoke.MethodHandle` on every one of them, so
    // there is no bytecode to fall back to and the comment above describes the
    // exact failure a retirement would restore. `H25-2` N3.
    r.register(
        mh,
        "rebind",
        "()Ljava/lang/invoke/BoundMethodHandle;",
        |_ctx, args| Ok(Some(args[0])), cratonvm_native_api::NativeKind::Bridge,
    );

    // `copyWith(MethodType, LambdaForm)` is JDK-abstract on `MethodHandle`
    // (the LambdaForm-backed subclasses implement it; CratonVM's synthetic
    // handles are none of them), so, as for `rebind` above, there is no
    // bytecode to shadow: the registration answers where the JDK would throw
    // `AbstractMethodError: ... copyWith ... has no Code attribute`.
    //
    // Its one caller that matters is `MethodHandle.viewAsType` (a new view of
    // the same behaviour under another type, no conversions), which the JDK's
    // `StringConcatFactory.makeConcatWithConstants` / `makeConcat` end with.
    // Those two run their own bytecode whenever they are CALLED rather than
    // linked by `invokedynamic` (the VM links javac's concat sites itself),
    // above all from `ObjectMethods.makeToString`, the `toString` of every
    // `ObjectMethods.bootstrap` site the native linkage hands to the JDK and of
    // every direct `ObjectMethods.bootstrap` call. All of those failed here
    // (interpreter round i1 wave 43, lane L4; probe
    // `L4W43CopyWithViews.java`).
    //
    // The view is `asType`'s pure copy (`mh_with_stamped_type`): the same
    // handle slots, the new `type`. The `LambdaForm` argument is ignored — a
    // synthetic handle has none, and its behaviour lives in its slots.
    r.register(
        mh,
        "copyWith",
        "(Ljava/lang/invoke/MethodType;Ljava/lang/invoke/LambdaForm;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| match (args.first(), args.get(1)) {
            (Some(Value::Object(Some(this))), Some(Value::Object(Some(mt)))) => {
                mh_with_stamped_type(ctx, Value::Object(Some(*this)), *mt, false)
            }
            // `copyWith(null, lf)`: the JDK's species constructors store the
            // type unchecked; no JDK caller passes null. Keep the receiver.
            (Some(this), _) => Ok(Some(*this)),
            (None, _) => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // type() — return a MethodType representing this MH's signature.
    // C19: prefer the real-JDK `type` field (populated by C15/C19 at
    // slot 0). Fall back to our synthetic descriptor, then finally
    // synthesize `()V` so callers never observe null.
    r.register(
        mh,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") {
                return Ok(Some(Value::Object(Some(mt))));
            }
            let desc = mh_read_desc(ctx, this).unwrap_or_default();
            if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &desc) {
                return Ok(Some(Value::Object(Some(mt))));
            }
            let mt = build_method_type_from_descriptor(ctx, "()V")?;
            Ok(Some(Value::Object(mt)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Lookup.find* are already registered in register_p63_method_handles_lookup
    // with full descriptor resolution. No duplicate registration needed here.

    // MethodType.makeImpl(Class, Class[], boolean): a reviewed `Intrinsic` over
    // concrete JDK bytecode (AGENTS.md), registered ONLY under
    // `CRATONVM_MH_METHOD_TYPE_INTERN_MEMO` (interpreter round i3 wave 74, lane
    // L4; default off), and since wave 75 only in a VM without a JIT
    // (`NativeMethodRegistry::jit_enabled`). A memo hit answers the interned instance the body
    // would; every other call IS the body (`mt_make_impl_intrinsic`). Removal:
    // when the interpreter's own invocation cost makes the body cheap enough
    // (the bench rows `mt-*` of `L4W74FinderMintSplitBench` decide), or never
    // if it is flipped on.
    if mh_method_type_intern_memo_registers(r) {
        r.register(
            MT_INTERN_CLASS,
            "makeImpl",
            MT_INTERN_DESC,
            mt_make_impl_intrinsic,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        // Wave 80: a no-op unless `CRATONVM_INTRINSIC_BODY_COMPILES`.
        r.mark_body_compilable_intrinsic_row(
            MT_INTERN_BODY_ROW,
            MT_INTERN_CLASS,
            "makeImpl",
            MT_INTERN_DESC,
        );
    }
    // Interpreter round i6 wave 132, lane L4 (`CRATONVM_MH_INVOKER_CHECKS_INTRINSIC`,
    // default off, no-JIT VMs only): `Invokers.checkCustomized` and
    // `Invokers.checkExactType` as reviewed `Intrinsic`s over JDK bytecode
    // (AGENTS.md); every call they do not answer IS the body
    // (`invokers_check_jdk_body`). See the section comment above
    // `INVOKERS_CLASS`. Removal: as `makeImpl`'s.
    if mh_invoker_checks_registers(r) {
        r.register(
            INVOKERS_CLASS,
            "checkCustomized",
            INVOKERS_CHECK_CUSTOMIZED_DESC,
            invokers_check_customized_intrinsic,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        r.register(
            INVOKERS_CLASS,
            "checkExactType",
            INVOKERS_CHECK_EXACT_TYPE_DESC,
            invokers_check_exact_type_intrinsic,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        r.mark_body_compilable_intrinsic_row(
            INVOKERS_CHECKS_BODY_ROW,
            INVOKERS_CLASS,
            "checkCustomized",
            INVOKERS_CHECK_CUSTOMIZED_DESC,
        );
        r.mark_body_compilable_intrinsic_row(
            INVOKERS_CHECKS_BODY_ROW,
            INVOKERS_CLASS,
            "checkExactType",
            INVOKERS_CHECK_EXACT_TYPE_DESC,
        );
    }
    // Interpreter round i7 wave 137, lane L4 (`CRATONVM_MH_LF_LEAF_INTRINSICS`,
    // default off, no-JIT VMs only): `MethodHandleImpl.checkSpreadArgument` and
    // `DirectMethodHandle.internalMemberName` as reviewed `Intrinsic`s over JDK
    // bytecode (AGENTS.md); every call they do not answer IS the body
    // (`invoke_special_bytecode_only`). See the section comment above `LF_LEAF_MHI_CLASS`.
    if mh_lf_leaf_registers(r) {
        r.register(
            LF_LEAF_MHI_CLASS,
            "checkSpreadArgument",
            LF_LEAF_CHECK_SPREAD_DESC,
            mhi_check_spread_argument_intrinsic,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        r.register(
            LF_LEAF_DMH_CLASS,
            "internalMemberName",
            LF_LEAF_INTERNAL_MEMBER_DESC,
            dmh_internal_member_name_intrinsic,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        r.mark_body_compilable_intrinsic_row(
            LF_LEAF_BODY_ROW,
            LF_LEAF_MHI_CLASS,
            "checkSpreadArgument",
            LF_LEAF_CHECK_SPREAD_DESC,
        );
        r.mark_body_compilable_intrinsic_row(
            LF_LEAF_BODY_ROW,
            LF_LEAF_DMH_CLASS,
            "internalMemberName",
            LF_LEAF_INTERNAL_MEMBER_DESC,
        );
    }
    // Interpreter round i6 wave 121, lane L2 (page
    // `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`):
    // the `Intrinsic`s above whose natives re-enter the JDK's own bytecode
    // (`mh_foreign_as_type` / `mh_real_as_type_bytecode`, `mh_iwa_jdk_body`,
    // `mh_foreign_bind_to`) name those bodies for re-entry-only publication.
    // Moved here from `vm_init.rs`, where wave 117 filed the list. Changes no
    // row's kind; the VM publishes nothing unless
    // `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`.
    r.mark_reentry_only_body(
        mh,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
    );
    r.mark_reentry_only_body(
        mh,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    );
    r.mark_reentry_only_body(mh, "invokeWithArguments", "(Ljava/util/List;)Ljava/lang/Object;");
    r.mark_reentry_only_body(
        mh,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
    );
    }

/// The `Class` mirror for one component of a method descriptor, named as
/// [`descriptor_to_class_name`] spells it (`int`, `java/lang/String`, `[I`, …).
///
/// A `MethodType`'s parameter and return mirrors are the objects
/// `java.lang.invoke` reasons about — `MethodTypeForm.canonicalize` erases a
/// parameter to `Object` only when `!t.isPrimitive()` — so a mirror that
/// misreports its own primitiveness corrupts every `MethodType` derived from
/// this one. Resolve the real class first, and only then fall back to the
/// VM's stand-in mirror factory.
///
/// The fallback is unavoidable: `class_id_by_name` answers `None` both for a
/// name no loader has and for a name SEVERAL loaders define (a Groovy script
/// class under `GroovyClassLoader$InnerLoader` is routinely both), and there
/// is still a descriptor to spell. `class_id_by_name_delegated` is tried in
/// between because it resolves the parent-delegation answer for an
/// ordinary-application lookup where the plain search declines to pick.
pub fn mirror_for_descriptor_name(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    mirror_for_descriptor_name_resolving(ctx, name, None).0
}

/// What [`mirror_for_descriptor_name_resolving`] answered a component with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorComponent {
    /// A primitive (or `void`) mirror.
    Primitive,
    /// The mirror of this loaded class (its raw `ClassId`).
    Class(u32),
    /// A stand-in: no lookup found the class.
    StandIn,
}

/// [`mirror_for_descriptor_name`], also telling what the mirror is (JIT round
/// 16 wave 15, lane mhapp): the bound-type table keys on the `ClassId`s, and
/// the builder interns only a type with no stand-in.
pub fn mirror_for_descriptor_name_resolving(
    ctx: &mut dyn NativeContext,
    name: &str,
    near: Option<ClassId>,
) -> (ObjectRef, DescriptorComponent) {
    // The nine primitive names are not classes; `primitive_class_mirror` is
    // their canonical factory and no lookup can improve on it.
    if is_primitive_descriptor_name(name) {
        return (
            ctx.primitive_class_mirror(name),
            DescriptorComponent::Primitive,
        );
    }
    // A lambda proxy's own name, as `unnameable_mirror_descriptor` renders
    // it (interpreter round i4 wave 92, lane L4): the proxy's mirror, not a
    // stand-in of that name. First, so no loader is asked for a name no
    // loader defines (a `$$Lambda/0x` name is never a loadable binary name).
    if let Some(cid) = lambda_proxy_descriptor_class(&*ctx, name) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    // Round 16 wave 17 (mhapp): the class `near`'s defining loader sees under
    // `name`, before the loader-blind lookups (JVMS 5.3: a descriptor's names
    // are resolved by the referencing class's loader).
    if let Some(cid) = near.and_then(|n| mh_caller_sees(&*ctx, name, n)) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    if let Some(cid) = ctx.class_id_by_name(name) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    if let Some(cid) = ctx.class_id_by_name_delegated(name) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    // An array descriptor is the one miss worth acting on: `[Ljava/lang/
    // Object;` is very often not yet indexed even though every ingredient
    // for it is, and the stand-in it would otherwise get has no
    // `componentType`, so `Class.getSimpleName()` renders `Object;` and
    // `mt.parameterType(0) == Object[].class` is false — the identity
    // `MethodHandle.asType` and Spring's converter registry both compare on.
    // Load it on demand, exactly as `native_class_array_type` does for
    // `Class.arrayType()`, and only then mint a stand-in.
    if name.starts_with('[') {
        let _ = ctx.load_class(name);
        if let Some(cid) = near
            .and_then(|n| mh_caller_sees(&*ctx, name, n))
            .or_else(|| ctx.class_id_by_name(name))
        {
            return (
                ctx.get_class_mirror(cid),
                DescriptorComponent::Class(cid.as_u32()),
            );
        }
    }
    (
        ctx.primitive_class_mirror(name),
        DescriptorComponent::StandIn,
    )
}

/// `CRATONVM_MH_INTERN_DESCRIPTOR_TYPES` -- default ON (JIT round 16 wave 15,
/// lane mhapp; a correctness fix, both modes): a `MethodType` the VM builds
/// from a descriptor ([`build_method_type_from_descriptor`]: `findVirtual`'s
/// type, `bindTo`'s bound type, ...) is the JDK's INTERNED instance, through
/// `MethodType.methodType(Class, Class[], boolean)` and so `makeImpl`'s
/// `internTable`, as on HotSpot, where two equal types are one object
/// (`R16WmteBindApp`: `same=true`). It was a fresh native carrier per build,
/// so `mh.bindTo(a).type() == mh.bindTo(b).type()` was false for every type
/// the bound-type table does not share. `0`/`false`/`off`/`no` restores the
/// fresh carrier. Read once (a test's thread override is honoured).
pub fn mh_intern_descriptor_types_enabled() -> bool {
    const KEY: &str = "CRATONVM_MH_INTERN_DESCRIPTOR_TYPES";
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if cratonvm_types::flags::overrides_active() {
        return cratonvm_types::flags::runtime_flag_default_on(KEY);
    }
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on(KEY))
}

/// `MethodType.methodType(Class, Class[], boolean)`: the package-private
/// "trusted" factory every public `methodType` overload ends in. It answers
/// the `objectOnlyTypes` cache for an all-`Object` type and `makeImpl`'s
/// interned instance otherwise. Not registered as a native anywhere, so the
/// call reaches the JDK's bytecode.
pub const MT_TRUSTED_FACTORY: &str =
    "(Ljava/lang/Class;[Ljava/lang/Class;Z)Ljava/lang/invoke/MethodType;";

/// The JDK's interned `MethodType` for the pinned `rtype` / `ptypes` (a fresh
/// `Class[]` the caller built and never writes again, so it is passed as
/// TRUSTED: on an intern miss the JDK keeps it as the new type's `ptypes`).
/// `None` -- the caller keeps its native carrier -- when the image has no
/// such factory (a synthetic JDK), `MethodType` is not initialized yet (a
/// build during its own `<clinit>`, or before it: this never runs a class
/// initializer), or the call does not answer a `MethodType`. Every failure is
/// dropped: the descriptor came from a loaded method, and the carrier is what
/// the build has always answered.
pub fn intern_descriptor_type_via_jdk(
    ctx: &mut dyn NativeContext,
    rtype_pin: usize,
    rtype: ObjectRef,
    ptypes_pin: usize,
    ptypes: ObjectRef,
) -> Option<ObjectRef> {
    let mt_class = ctx.class_id_by_name(MT_CLASS)?;
    if !ctx.is_class_initialized(mt_class)
        || ctx.is_class_synthetic_stub(MT_CLASS)
        || !ctx.method_exists(MT_CLASS, "methodType", MT_TRUSTED_FACTORY)
    {
        return None;
    }
    let rtype = ctx.read_native_pin(rtype_pin, rtype);
    let ptypes = ctx.read_native_pin(ptypes_pin, ptypes);
    match ctx.invoke(
        MT_CLASS,
        "methodType",
        MT_TRUSTED_FACTORY,
        &[
            Value::Object(Some(rtype)),
            Value::Object(Some(ptypes)),
            Value::Int(1),
        ],
    ) {
        Ok(Some(Value::Object(Some(mt)))) => Some(mt),
        _ => None,
    }
}

/// Build a MethodType object from a JVM method descriptor string.
pub fn build_method_type_from_descriptor(
    ctx: &mut dyn NativeContext,
    desc: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    Ok(build_method_type_from_descriptor_resolving(ctx, desc, None)?.map(|(mt, _)| mt))
}

/// [`build_method_type_from_descriptor`] resolving every class name through
/// `near`'s defining loader first (round 16 wave 17, lane mhapp); `None` is
/// the loader-blind builder.
pub fn build_method_type_from_descriptor_near(
    ctx: &mut dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    Ok(build_method_type_from_descriptor_resolving(ctx, desc, near)?.map(|(mt, _)| mt))
}

/// [`build_method_type_from_descriptor`], also answering the raw `ClassId`
/// it resolved for every reference component (parameters then the return,
/// primitives skipped), or `None` there when one is a stand-in.
pub fn build_method_type_from_descriptor_resolving(
    ctx: &mut dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Result<Option<(ObjectRef, Option<Vec<u32>>)>, MethodCallFailed> {
    if desc.is_empty() || !desc.starts_with('(') {
        return Ok(None);
    }

    let Some(close) = desc_close_paren(desc) else {
        return Ok(None);
    };
    let params_str = &desc[1..close];
    let ret_str = &desc[close + 1..];

    let param_names = parse_descriptor_types(params_str);
    let ret_name = descriptor_to_class_name(ret_str);

    // Create return type Class mirror
    let (ret_mirror, ret_resolved) = mirror_for_descriptor_name_resolving(ctx, &ret_name, near);
    let mut resolved: Option<Vec<u32>> = Some(Vec::with_capacity(param_names.len() + 1));
    let note = |r: DescriptorComponent, resolved: &mut Option<Vec<u32>>| match r {
        DescriptorComponent::Primitive => {}
        DescriptorComponent::Class(id) => {
            if let Some(ids) = resolved.as_mut() {
                ids.push(id);
            }
        }
        DescriptorComponent::StandIn => *resolved = None,
    };

    // GC-safety: `new_array`/`primitive_class_mirror` (lazily allocates a
    // synthetic mirror on first use, same as `synthetic_class_mirror`)/
    // `alloc_concurrent_synthetic` below can all trigger a collection that
    // relocates `ret_mirror`; pin it and re-read the forwarded reference
    // before each subsequent use.
    let ret_mirror_pin = ctx.pin_native_root(ret_mirror);

    // Create params array: a `Class[]`, `MethodType.ptypes`' declared type.
    // It was an untyped `Object[]` until wave 45 (lane L4), and every JDK
    // `(Class<?>[]) Arrays.copyOf(ptypes, ..)` over such a type failed its
    // `checkcast` (`MethodType.asCollectorType` / `insertParameterTypes`;
    // `L4W44RecordDeserialization`, `L4W44CombinatorHunt` `asCollector-pos`).
    let arr = new_class_array(ctx, param_names.len());
    let ret_mirror = ctx.read_native_pin(ret_mirror_pin, ret_mirror);
    // Same GC-safety concern applies to `arr`, written into on every loop
    // iteration after a per-iteration `primitive_class_mirror`/allocating
    // call; pin it too and re-read before each write.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, pname) in param_names.iter().enumerate() {
        let (mirror, r) = mirror_for_descriptor_name_resolving(ctx, pname, near);
        note(r, &mut resolved);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    note(ret_resolved, &mut resolved);

    // JIT round 16 wave 15 (mhapp): the JDK's interned instance, as HotSpot
    // answers (`mh_intern_descriptor_types_enabled`). Never over a stand-in:
    // interning one would freeze it into the JDK's table.
    if resolved.is_some() && mh_intern_descriptor_types_enabled() {
        if let Some(mt) =
            intern_descriptor_type_via_jdk(ctx, ret_mirror_pin, ret_mirror, arr_pin, arr)
        {
            ctx.unpin_native_roots(ret_mirror_pin);
            return Ok(Some((mt, resolved)));
        }
    }

    // JDK MethodType field layout: rtype(0), ptypes(1), form(2), wrapAlt(3),
    // invokers(4), methodDescriptor(5). Allocate 6 slots so the JDK-resolved
    // `form` slot (2) lives within the synthetic object.
    let mt: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6)?;
    let ret_mirror = ctx.read_native_pin(ret_mirror_pin, ret_mirror);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(ret_mirror_pin);
    ctx.set_field(mt, 0, Value::Object(Some(ret_mirror)));
    ctx.set_field(mt, 1, Value::Object(Some(arr)));

    // gen r4w3/rooting: `populate_method_type_form` refreshes `mt` in place.
    let mut mt = mt;
    populate_method_type_form(ctx, &mut mt)?;
    Ok(Some((mt, resolved)))
}

/// C21: Build a synthetic MethodTypeForm and link it into `mt.form` so
/// `MethodType.parameterSlotCount()` (= `form.parameterSlotCount`) and
/// `MethodType.erasedType()` / `.erase()` (= `form.erasedType()`) do not NPE
/// on the null `form` field. JDK MethodTypeForm field layout:
///   parameterSlotCount: short (slot 0)
///   primitiveCount:     short (slot 1)
///   erasedType:         MethodType (slot 2)
///   basicType:          MethodType (slot 3)
///   methodHandles:      SoftReference[] (slot 4)
///   lambdaForms:        SoftReference[] (slot 5)
///   interpretEntry:     SoftReference (slot 6)
///
/// Reads the `ptypes` array (slot 1) to compute slot/primitive counts and
/// stores `mt` itself as both erasedType and basicType — a minimal stand-in
/// so downstream `form.erasedType()` returns a non-null MethodType.
///
/// Slots 4/5 (`methodHandles`, `lambdaForms`) are the JDK's two lazy caches
/// and MUST be non-null arrays here — see `MTF_LF_CACHE_LEN` below.
///
/// gen r4w3/rooting: `mt` is `&mut` because the form allocation below is a GC
/// point (`try_alloc_concurrent_synthetic` can run `<clinit>`); the caller's
/// copy of `mt` is refreshed to its post-collection address on success.
pub fn populate_method_type_form(
    ctx: &mut dyn NativeContext,
    mt_ref: &mut cratonvm_types::ObjectRef,
) -> Result<(), MethodCallFailed> {
    let mt = *mt_ref;
    let mut slot_count: i32 = 0;
    let mut primitive_count: i32 = 0;
    if let Value::Object(Some(arr)) = ctx.get_field(mt, 1) {
        let n = ctx.array_length(arr);
        for i in 0..n {
            slot_count += 1;
            if let Value::Object(Some(mirror)) = ctx.get_array_element(arr, i) {
                let nm = crate::lang_class::mirror_class_name(ctx, mirror).unwrap_or_default();
                match nm.as_str() {
                    "long" | "double" => slot_count += 1,
                    "boolean" | "byte" | "char" | "short" | "int" | "float" => {
                        primitive_count += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    // GC-safety: `alloc_concurrent_synthetic` and the two cache-array
    // allocations below can each trigger a collection that relocates `mt` and `form`.
    // Pin both and re-read the forwarded reference before every use.
    let mt_pin = ctx.pin_native_root(mt);
    let form = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodTypeForm", 7)?;
    let form_pin = ctx.pin_native_root(form);
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field(form, 0, Value::Int(slot_count));
    ctx.set_field(form, 1, Value::Int(primitive_count));
    ctx.set_field(form, 2, Value::Object(Some(mt)));
    ctx.set_field(form, 3, Value::Object(Some(mt)));
    // Slots 4/5 are the JDK's two lazy caches, `methodHandles` and
    // `lambdaForms`. Leaving them null is a state the real constructor NEVER
    // produces for a form that claims `basicType == erasedType` — which is
    // exactly what the two writes above claim for this form. The real
    // `MethodTypeForm(MethodType)` allocates BOTH arrays on that branch and
    // only leaves them null on the non-basic branch, whose accessors the JDK
    // then never calls (every caller routes through `basicType().form()`).
    //
    // Every JDK reader is a bare indexed load with no null check:
    //   MethodTypeForm.cachedLambdaForm   -> `lambdaForms[which]`
    //   MethodTypeForm.cachedMethodHandle -> `methodHandles[which]`
    // so a null slot is not a cache miss, it is a NullPointerException in
    // real-JDK bytecode we cannot patch. Observed twice:
    //   * `CallSite.makeUninitializedCallSite` NPE'd on the null
    //     `methodHandles` — worked around by shimming that method instead
    //     (see the `makeUninitializedCallSite` registration below).
    //   * `MethodHandle.asVarargsCollector` -> `DelegatingMethodHandle
    //     .makeReinvokerForm` -> `mtype.form().cachedLambdaForm(LF_DELEGATE)`
    //     NPE'd with `Cannot load from object array because "this.lambdaForms"
    //     is null` (regression-suite RJdkHandles, BOTH --fast-boot and
    //     --real-jdk; HotSpot 25 passes).
    // Allocating empty arrays turns those reads back into ordinary cache
    // MISSES (null entry), which is what the JDK expects on a fresh form.
    // Both fields are declared `SoftReference<...>[]` (i6-L2: typed, not
    // `Object[]`).
    let soft_ref = crate::lang_class::reflection_component_id(ctx, "java/lang/ref/SoftReference");
    let method_handles = ctx.new_ref_array(soft_ref, MTF_MH_CACHE_LEN);
    let form = ctx.read_native_pin(form_pin, form);
    ctx.set_field(form, 4, Value::Object(Some(method_handles)));
    let lambda_forms = ctx.new_ref_array(soft_ref, MTF_LF_CACHE_LEN);
    let form = ctx.read_native_pin(form_pin, form);
    ctx.set_field(form, 5, Value::Object(Some(lambda_forms)));
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field(mt, 2, Value::Object(Some(form)));
    ctx.unpin_native_roots(mt_pin);
    *mt_ref = mt;
    Ok(())
}

/// Length of the fabricated `MethodTypeForm.methodHandles` cache (slot 4).
/// JDK 25's `MethodTypeForm.MH_LIMIT` is 3.
///
/// Deliberately sized PAST the JDK constant. Both caches are `private` fields
/// touched only by `MethodTypeForm`'s own accessors, and only ever by an
/// indexed load/store — nothing reads `.length`, nothing iterates them — so an
/// over-long array is indistinguishable from an exact one to every reader,
/// while an array sized from a stale constant would be an
/// `ArrayIndexOutOfBoundsException` the day a JDK release adds a cache index.
pub const MTF_MH_CACHE_LEN: usize = 16;

/// Length of the fabricated `MethodTypeForm.lambdaForms` cache (slot 5).
/// JDK 25's `MethodTypeForm.LF_LIMIT` is 26. Over-sized for the reason given
/// on `MTF_MH_CACHE_LEN`.
pub const MTF_LF_CACHE_LEN: usize = 64;

/// Parse a sequence of JVM type descriptors from a parameter string.
/// e.g. "ILjava/lang/String;D" → ["int", "java/lang/String", "double"]
///
/// Returns `Cow<'static, str>` so the primitive descriptor branches
/// (which dominate this hot path on the invoke/invokeExact dispatch) avoid
/// allocating a `String` per parameter; only reference / array types
/// produce an owned string.
pub fn parse_descriptor_types(desc: &str) -> Vec<Cow<'static, str>> {
    let mut result: Vec<Cow<'static, str>> = Vec::new();
    let bytes = desc.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'I' => {
                result.push(Cow::Borrowed(NAME_INT));
                i += 1;
            }
            b'J' => {
                result.push(Cow::Borrowed(NAME_LONG));
                i += 1;
            }
            b'F' => {
                result.push(Cow::Borrowed(NAME_FLOAT));
                i += 1;
            }
            b'D' => {
                result.push(Cow::Borrowed(NAME_DOUBLE));
                i += 1;
            }
            b'Z' => {
                result.push(Cow::Borrowed(NAME_BOOLEAN));
                i += 1;
            }
            b'B' => {
                result.push(Cow::Borrowed(NAME_BYTE));
                i += 1;
            }
            b'C' => {
                result.push(Cow::Borrowed(NAME_CHAR));
                i += 1;
            }
            b'S' => {
                result.push(Cow::Borrowed(NAME_SHORT));
                i += 1;
            }
            b'V' => {
                result.push(Cow::Borrowed(NAME_VOID));
                i += 1;
            }
            b'L' => {
                if let Some(semi) = desc[i..].find(';') {
                    result.push(Cow::Owned(desc[i + 1..i + semi].to_string()));
                    i += semi + 1;
                } else {
                    break;
                }
            }
            b'[' => {
                let start = i;
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        if let Some(semi) = desc[i..].find(';') {
                            result.push(Cow::Owned(desc[start..i + semi + 1].to_string()));
                            i += semi + 1;
                        } else {
                            break;
                        }
                    } else {
                        result.push(Cow::Owned(desc[start..=i].to_string()));
                        i += 1;
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    result
}

