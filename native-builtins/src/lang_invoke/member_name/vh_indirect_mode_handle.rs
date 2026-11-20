#[allow(unused_imports)]
use super::*;

/// The mode handle for `vh` and the access mode `ordinal`: an indirect handle's
/// composed (and cached) adapter, or a direct handle's invoker.
pub fn vh_indirect_mode_handle(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    ordinal: usize,
    depth: u32,
) -> Result<ObjectRef, MethodCallFailed> {
    let method = VH_ACCESS_MODES[ordinal].1;
    if depth > VH_INDIRECT_MAX_DEPTH {
        return Err(vh_indirect_refusal(method, "combinator chain too deep"));
    }
    let indirect = is_indirect_var_handle(ctx, vh);
    if indirect {
        if let Some(mh) = vh_indirect_cached_mode_handle(ctx, vh, ordinal) {
            return Ok(mh);
        }
    }
    let base = ctx.pin_native_root(vh);
    let result = if indirect {
        vh_indirect_compose_mode_handle(ctx, base, vh, ordinal, depth)
    } else if vh_real_adoption_enabled() {
        // The family is real (SBR3 mhperf): the JDK's own mode handle, a real `MethodHandle`
        // the real combinators of the `handleFactory` can edit. The carrier invoker below is
        // not one (`NoSuchMethodError: MethodHandle.editor()` in `MethodHandles.collectArguments`).
        vh_real_mode_handle(ctx, base, vh, ordinal)
    } else {
        vh_direct_mode_invoker(ctx, base, vh, ordinal)
    };
    ctx.unpin_native_roots(base);
    result
}

/// `target.getMethodHandle(ordinal)`: HotSpot's own mode handle of a direct `VarHandle`, typed
/// `(VarHandle, accessModeType(mode)...)`, unbound (the JDK's `IndirectVarHandle.
/// getMethodHandleUncached` asks the same of its target). The caller pinned `vh`.
pub fn vh_real_mode_handle(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let method = VH_ACCESS_MODES[ordinal].1;
    let vh = ctx.read_native_pin(vh_pin, vh);
    match ctx.invoke_virtual_bytecode_only(
        vh,
        "getMethodHandle",
        "(I)Ljava/lang/invoke/MethodHandle;",
        &[Value::Int(ordinal as i32)],
    )? {
        Some(Value::Object(Some(mh))) => Ok(mh),
        _ => Err(vh_indirect_refusal(method, "getMethodHandle answered null")),
    }
}

pub fn vh_indirect_cached_mode_handle(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    ordinal: usize,
) -> Option<ObjectRef> {
    let idx = ctx.resolve_field_index(VAR_HANDLE_CLASS, "methodHandleTable")?;
    let Value::Object(Some(table)) = ctx.get_field(vh, idx) else {
        return None;
    };
    if !matches!(ctx.heap_kind_of(table), ObjectKind::Array) || ctx.array_length(table) <= ordinal {
        return None;
    }
    match ctx.get_array_element(table, ordinal) {
        Value::Object(Some(mh)) => Some(mh),
        _ => None,
    }
}

/// `handleFactory.apply(mode, <target's mode handle>)`, cached. The caller
/// pinned `vh` at `vh_pin` and unpins everything pinned here.
pub fn vh_indirect_compose_mode_handle(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
    depth: u32,
) -> Result<ObjectRef, MethodCallFailed> {
    let (mode_name, method) = VH_ACCESS_MODES[ordinal];
    let Some(target) = indirect_vh_field(ctx, vh, "target") else {
        return Err(vh_indirect_refusal(method, "the handle has no target"));
    };
    let target_mh = vh_indirect_mode_handle(ctx, target, ordinal, depth + 1)?;
    let target_mh_pin = ctx.pin_native_root(target_mh);
    let mode = vh_access_mode_constant(ctx, mode_name, method)?;
    let mode_pin = ctx.pin_native_root(mode);
    let vh = ctx.read_native_pin(vh_pin, vh);
    let Some(factory) = indirect_vh_field(ctx, vh, "handleFactory") else {
        return Err(vh_indirect_refusal(
            method,
            "the handle has no handleFactory",
        ));
    };
    let target_mh = ctx.read_native_pin(target_mh_pin, target_mh);
    let mode = ctx.read_native_pin(mode_pin, mode);
    let mh = match ctx.invoke_virtual(
        factory,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(mode)), Value::Object(Some(target_mh))],
    )? {
        Some(Value::Object(Some(mh))) => mh,
        _ => {
            return Err(vh_indirect_refusal(
                method,
                "the handleFactory answered null",
            ))
        }
    };
    let mh_pin = ctx.pin_native_root(mh);
    vh_indirect_cache_mode_handle(ctx, vh_pin, vh, ordinal, mh_pin, mh);
    Ok(ctx.read_native_pin(mh_pin, mh))
}

/// Store a composed mode handle in `vh`'s `methodHandleTable`, allocating the
/// table (a `MethodHandle[AccessMode.COUNT]`, as `VarHandle.getMethodHandle`
/// does) when absent. Only ever called for an `IndirectVarHandle`, whose
/// fields are the real class's: a CratonVM-minted handle imposes its own
/// meaning on the slots and must not be written here.
pub fn vh_indirect_cache_mode_handle(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
    mh_pin: usize,
    mh: ObjectRef,
) {
    let Some(idx) = ctx.resolve_field_index(VAR_HANDLE_CLASS, "methodHandleTable") else {
        return;
    };
    let vh = ctx.read_native_pin(vh_pin, vh);
    let table = match ctx.get_field(vh, idx) {
        Value::Object(Some(t)) => t,
        // No table yet (an unwritten reference slot may also read back as
        // `Int(0)`, the zeroed slot's niche).
        _ => {
            let t = match ctx.class_id_by_name("java/lang/invoke/MethodHandle") {
                Some(cid) => ctx.new_ref_array(cid, VH_ACCESS_MODES.len()),
                None => ctx.new_array(ArrayElementType::Reference, VH_ACCESS_MODES.len()),
            };
            let vh = ctx.read_native_pin(vh_pin, vh);
            ctx.set_field(vh, idx, Value::Object(Some(t)));
            t
        }
    };
    if !matches!(ctx.heap_kind_of(table), ObjectKind::Array) || ctx.array_length(table) <= ordinal {
        return;
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.set_array_element(table, ordinal, Value::Object(Some(mh)));
}

/// A direct handle's mode invoker: the access mode's method on
/// `java/lang/invoke/VarHandle`, typed `(VarHandle, accessModeType(mode)...)`,
/// unbound -- `varhandle_to_method_handle`'s handle before its `bindTo`, and
/// HotSpot's `VarHandle.getMethodHandleUncached`. The caller pinned `vh`.
pub fn vh_direct_mode_invoker(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let (mode_name, method) = VH_ACCESS_MODES[ordinal];
    let mode = vh_access_mode_constant(ctx, mode_name, method)?;
    let vh = ctx.read_native_pin(vh_pin, vh);
    let mode_type = match ctx.invoke_virtual(
        vh,
        "accessModeType",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(mode))],
    )? {
        Some(Value::Object(Some(mt))) => mt,
        _ => {
            return Err(vh_indirect_refusal(
                method,
                "the target has no access-mode type",
            ))
        }
    };
    let desc = match ctx.invoke_virtual(
        mode_type,
        "toMethodDescriptorString",
        "()Ljava/lang/String;",
        &[],
    )? {
        Some(Value::Object(Some(d))) => ctx.read_string(d).unwrap_or_default(),
        _ => String::new(),
    };
    if desc.is_empty() {
        return Err(vh_indirect_refusal(
            method,
            "the target's access-mode type has no descriptor",
        ));
    }
    alloc_method_handle(ctx, VAR_HANDLE_CLASS, method, &desc, MH_KIND_VIRTUAL)
}

/// The `VarHandle.AccessMode` constant named `mode_name` (`GET`, ...).
pub fn vh_access_mode_constant(
    ctx: &mut dyn NativeContext,
    mode_name: &str,
    method: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let name = ctx.create_string_uninterned(mode_name);
    match ctx.invoke(
        "java/lang/invoke/VarHandle$AccessMode",
        "valueOf",
        "(Ljava/lang/String;)Ljava/lang/invoke/VarHandle$AccessMode;",
        &[Value::Object(Some(name))],
    )? {
        Some(Value::Object(Some(mode))) => Ok(mode),
        _ => Err(vh_indirect_refusal(
            method,
            "no VarHandle.AccessMode constant",
        )),
    }
}

/// `varType()` of an `IndirectVarHandle`: its own `value` field, which is
/// what HotSpot's `accessModeTypeUncached` returns for `GET`. Without it a
/// combinator over a combinator (`insertCoordinates(filterValue(..), ..)`)
/// failed at construction: every combinator asks its target `varType()`.
pub fn indirect_vh_var_type(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<ObjectRef> {
    if !vh_indirect_serve_enabled() || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    indirect_vh_field(ctx, vh, "value")
}

/// `coordinateTypes()` of an `IndirectVarHandle`: `List.of(coordinates)` (the
/// list copies the array), for the reason [`indirect_vh_var_type`] gives.
pub fn indirect_vh_coordinate_types(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<Value> {
    if !vh_indirect_serve_enabled() || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    let coords = indirect_vh_field(ctx, vh, "coordinates")?;
    match ctx.invoke(
        "java/util/List",
        "of",
        "([Ljava/lang/Object;)Ljava/util/List;",
        &[Value::Object(Some(coords))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// `IndirectVarHandle`: round 15 wave 1 (lane vh)
// ---------------------------------------------------------------------------
//
// `r14w7-ffm7-indirect-var-handle-residuals-FIXED-20260930.md`:
//
// * item 3: `MethodHandles.filterValue` composes its GET / GET_AND_UPDATE /
//   COMPARE_AND_EXCHANGE mode handles with the package-private
//   `MethodHandles.collectReturnValue`, whose JDK body is
//   `target.rebind().editor()...`: a CratonVM handle has no `LambdaForm`
//   editor (`NoSuchMethodError: MethodHandle.editor()`). Served here for
//   every caller, as the composition of two combinators this file already
//   serves ([`mhs_collect_return_value`]).
// * item 1: an exact `IndirectVarHandle` (its real `withInvokeExactBehavior`
//   bytecode) never opened the doors' exact-handle gate, and
//   `varhandle_exact_call_site_refusal` could not describe it
//   ([`indirect_vh_note_exact`], [`indirect_vh_exact_access_descriptor`]).
// * item 2: an `int` slot narrowed to a `char`/`short`/`byte`/`boolean` value
//   parameter, judged from the value-site cell
//   ([`vh_indirect_value_site_refusal`]).
// * proposal F7-1 (`jit-r14-ffm7-proposals-RETIRED-20260930.md`): the mode handle invoked at its
//   own type through the `invokeExact` door, with no `Object[]` and no
//   argument boxes ([`vh_indirect_typed_args`]), opt-in.

/// `CRATONVM_MH_COLLECT_RETURN_VALUE` (default on, both modes; round 15 wave
/// 1, lane vh): `MethodHandles.collectReturnValue` is served by
/// [`mhs_collect_return_value`]. `0` runs the JDK body again (the
/// `NoSuchMethodError: MethodHandle.editor()` of every `filterValue` read).
pub fn mh_collect_return_value_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_VH_INDIRECT_EXACT` (default on; round 15 wave 1, lane vh): an
/// exact `IndirectVarHandle` is judged at the call site as HotSpot judges it
/// (`WrongMethodTypeException` for a type that is not exactly the access
/// mode's), in the modes [`vh_exact_judged_in_this_mode`] admits. `0`
/// restores the unjudged access.
pub fn vh_indirect_exact_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| true)
}

/// `CRATONVM_VH_EXACT_COMPATIBLE` (default on; round 15 wave 1 hand-back, lane
/// vh): `--compatible` answers `withInvokeExactBehavior()` /
/// `withInvokeBehavior()` and judges exact handles as `--real-jdk` does
/// ([`varhandle_with_exactness`], [`indirect_vh_note_exact`]). `0` restores
/// the pre-round-15 `--compatible` answer (`this`, never judged). No effect
/// under `--real-jdk`.
pub fn vh_exact_compatible_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Whether this VM's policy mints exact `VarHandle`s and opens the doors'
/// exact-handle gate: `--real-jdk` always, `--compatible` unless
/// [`vh_exact_compatible_enabled`] is off.
pub fn vh_exact_judged_in_this_mode(ctx: &dyn NativeContext) -> bool {
    // `JdkInvoke` (nio21 wave 12, L3): `--compatible` with the row takes
    // `--real-jdk`'s arm (`aligned` is always true under `--real-jdk`).
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke) || vh_exact_compatible_enabled()
}

/// `CRATONVM_VH_INDIRECT_SITE_COORD_WMTE` (default on, both modes; round 15
/// wave 1 hand-back, lane vh): the COORDINATE parameters the value-site cell
/// covers (a store mode's last coordinate) are judged like the values
/// ([`vh_indirect_value_site_refusal`]). `0` converts them permissively again.
pub fn vh_indirect_site_coord_wmte_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_VH_INDIRECT_SITE_ALL_COORDS` (default on, both modes; round 15
/// wave 3, lane vh3; VH1-3): [`vh_indirect_value_site_refusal`] judges every
/// call-site parameter the parameter-site word covers (up to
/// `poly_call_site::VH_PARAM_SITE_SLOTS`), so a read mode's coordinates and a
/// store mode's earlier coordinates are judged as its last one is. `0` judges
/// the two low positions only (the round-15-wave-1 reach).
pub fn vh_indirect_site_all_coords_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_VH_MIC_EXACT_JUDGE` (default on, both modes; round 15 wave 3,
/// lane vh3; `r15w2-vh2-thin-helpers-bound-before-first-exact-mint-FIXED-20260930.md`):
/// the compiled-code virtual-call door (`vm::jit::helpers::
/// jit_invoke_virtual_mic`) judges an invoke-exact `VarHandle` against the
/// site's real descriptor once the native site cache has declined it, as the
/// generic dispatch door already does. `0` lets that door's tail run the
/// access unjudged again (the round-15-wave-2 answer). Cached here because the
/// door asks on every such access once an exact handle exists, and `vm/src`
/// may not grow a `static`.
pub fn vh_exact_mic_judge_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_VH_THIN_BIND_EXACT_GATE` (default on; round 15 wave 2, lane
/// vh2), cached (round 15 wave 3, lane vh3): `vm::jit::helpers::
/// varhandle_thin_binds_admitted` asks it on every compile once the VM has an
/// exact handle, and `vm/src` may not grow a `static`. Same name, same
/// default, same meaning as before.
pub fn vh_thin_bind_exact_gate_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_OSR_VH_BIND_ASKS_ADMISSION` (default on; round 15 wave 4,
/// lane vh5 patch, applied by the orchestrator): under `--real-jdk` the OSR
/// door binds the thin `VarHandle` helpers when the registry admits the
/// `Bridge` bind, as the MethodEntry door does. `0` refuses them under
/// `--real-jdk` again.
pub fn vh_osr_bind_asks_admission_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_SPLICE_VH_REFUSAL_ASKS_BIND` (default on, both modes; round
/// 15 wave 4, lane vh5; VH3-5,
/// `r15w3-vh3-splice-refusal-ignores-unwired-varhandle-binds-FIXED-20260930.md`):
/// the VM's splice planner refuses a callee for a `VarHandle` site only when
/// the callee's own compile will bind that site to a thin helper
/// (`vm::jit::helpers::varhandle_splice_keeps_thin_bind`), `compareAndSet`
/// included. `0` refuses every recognised read/write site again, bound or
/// not, and never a CAS one (the round-15-wave-3 answer). Cached here because
/// `vm/src` may not grow a `static`.
pub fn vh_splice_refusal_asks_bind_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_VH_INDIRECT_SITE_WMTE` (default on, both modes; round 15 wave 1,
/// lane vh): a value parameter of an `IndirectVarHandle` access whose static
/// call-site type `asType` cannot convert to the access mode's is refused
/// with `WrongMethodTypeException`. `0` restores the permissive boxing.
pub fn vh_indirect_site_wmte_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_VH_INDIRECT_TYPED=1` (opt-in; round 15 wave 1, lane vh; F7-1):
/// an access whose arguments already are the mode handle's declared types is
/// served through the `invokeExact` door instead of `invokeWithArguments`.
pub fn vh_indirect_typed_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)
}

/// The rows of this block, each with its kind stated here. Called from
/// `register_phase54_method_handle` (both boot paths).
///
/// One `Bridge` standing in front of real bytecode:
/// `MethodHandles.collectReturnValue`, because its body needs a `LambdaForm`
/// editor no CratonVM handle has
/// (`docs/feature-designs/native-dispatch-architecture.md` says what retires it).
///
/// `IndirectVarHandle.withInvokeExactBehavior` is no longer registered in either
/// mode: SBR5 FIX3 (2026-10-07) retired it under `--real-jdk` (the doors resolve
/// the `exact` slot on their first ask, `var_handle_exact_gate`), and the
/// `--compatible` row went the same way once the gate was shown mode-independent.
pub fn register_r15_vh_indirect_natives(r: &mut NativeMethodRegistry) {
    r.register(
        "java/lang/invoke/MethodHandles",
        "collectReturnValue",
        MHS_COLLECT_RETURN_VALUE_DESC,
        mhs_collect_return_value,
        cratonvm_native_api::NativeKind::Bridge,
    );
}

pub const MHS_COLLECT_RETURN_VALUE_DESC: &str =
    "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;";

/// How [`mhs_collect_return_value`] builds `collectReturnValue(target,
/// filter)` for `T target(A...)` and `V filter(B..., T)`: the adapter is
/// `V adapter(A... a, B... b) { return filter(b..., target(a...)); }`.
///
/// `collectArguments(filter, n, target)` is `(B..., A...)V` with exactly that
/// body (`n` = the count of `B`); for `n > 0` a `permuteArguments` to
/// `(A..., B...)V` gives the JDK's parameter order.
#[derive(Debug, PartialEq, Eq)]
pub struct CollectReturnValuePlan {
    /// `n`: the filter's leading parameters, the collect position.
    pub leading: i32,
    /// For `n > 0`: the adapter's descriptor and the reorder from the
    /// collected handle's `(B..., A...)` to it.
    pub permute: Option<(String, Vec<i32>)>,
}

/// The plan for a target typed `target_desc` and a filter typed
/// `filter_desc`, or `None` for a shape the JDK body alone decides (a filter
/// whose last parameter is not the target's return, a value filter over a
/// `void` target).
pub fn collect_return_value_plan(
    target_desc: &str,
    filter_desc: &str,
) -> Option<CollectReturnValuePlan> {
    let (tparams, tret) = split_descriptor_params(target_desc)?;
    let (fparams, fret) = split_descriptor_params(filter_desc)?;
    let Some((value, leading)) = fparams.split_last() else {
        // `V filter()` takes nothing from a `void` target.
        return (tret == DESC_VOID).then_some(CollectReturnValuePlan {
            leading: 0,
            permute: None,
        });
    };
    if tret == DESC_VOID || *value != tret {
        return None;
    }
    if leading.is_empty() {
        return Some(CollectReturnValuePlan {
            leading: 0,
            permute: None,
        });
    }
    let m = tparams.len();
    let n = leading.len();
    let desc = format!("({}{}){fret}", tparams.concat(), leading.concat());
    let mut reorder = Vec::with_capacity(n + m);
    for i in 0..n {
        reorder.push(i32::try_from(m + i).ok()?);
    }
    for j in 0..m {
        reorder.push(i32::try_from(j).ok()?);
    }
    Some(CollectReturnValuePlan {
        leading: i32::try_from(n).ok()?,
        permute: Some((desc, reorder)),
    })
}

/// `MethodHandles.collectReturnValue(target, filter)` (package-private; its
/// caller is `VarHandles.filterValue`'s handle factory): see
/// [`CollectReturnValuePlan`]. Anything it does not plan -- the switch off, a
/// null argument (the JDK body's `target.type()` NPE), an unreadable type --
/// runs the JDK body, today's answer.
pub fn mhs_collect_return_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let planned = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(t))), Some(Value::Object(Some(f))))
            if mh_collect_return_value_enabled() =>
        {
            let (target, filter) = (*t, *f);
            match (
                mh_type_descriptor(ctx, target),
                mh_type_descriptor(ctx, filter),
            ) {
                (Some(td), Some(fd)) => {
                    collect_return_value_plan(&td, &fd).map(|plan| (target, filter, plan))                }
                _ => None,            }        }
        _ => None,
    };
    let Some((target, filter, plan)) = planned else {
        return ctx.invoke_special_bytecode_only(
            "java/lang/invoke/MethodHandles",
            "collectReturnValue",
            MHS_COLLECT_RETURN_VALUE_DESC,
            args,
        );
    };
    // The two combinators' own bodies, called directly: a native-to-Java
    // `invoke_static` of a Bridge standing over bytecode takes the
    // class-shared door, where `--real-jdk` runs the JDK body instead.
    let collected = make_collect_args_adapter(
        ctx,
        Some(Value::Object(Some(filter))),
        plan.leading,
        Some(Value::Object(Some(target))),
    )?;
    let Some((desc, reorder)) = plan.permute else {
        return Ok(collected);
    };
    let collected = mh_handle_answer(collected, "collectArguments")?;
    let permuted = mh_permute_to_desc(ctx, collected, &desc, &reorder)?;
    Ok(Some(Value::Object(Some(permuted))))
}

/// Open the doors' exact-handle gate (`note_exact_var_handle_minted`, what
/// every exact handle this file mints does) for an exact `IndirectVarHandle`
/// the JDK built, in the modes [`vh_exact_judged_in_this_mode`] admits (see
/// [`varhandle_with_exactness`] for why `--compatible` is safe).
pub fn indirect_vh_note_exact(ctx: &mut dyn NativeContext, built: &Option<Value>) {
    if !vh_exact_judged_in_this_mode(ctx) || !vh_indirect_exact_enabled() {
        return;
    }
    let Some(Value::Object(Some(handle))) = built else {
        return;
    };
    let handle = *handle;
    if !is_indirect_var_handle(ctx, handle) {
        return;
    }
    let class_id = ctx.class_id_of_object(handle);
    let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "exact") else {
        return;
    };
    if matches!(ctx.get_field(handle, slot), Value::Int(v) if v != 0) {
        ctx.note_exact_var_handle_minted(slot);
    }
}

/// The descriptor a `Class` mirror names, or `None` when it cannot be read --
/// never `mirror_to_descriptor`'s `Object` fallback, which would invent a
/// mismatch at an exact call site.
///
/// Interpreter round i6 wave 118, lane L4 (follow-on of
/// `i62-L4-proposal-render-an-unnameable-mirror-as-itself-not-object`, under its
/// `CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME`): a lambda proxy's mirror, which
/// `resolve_class_name_robust` cannot name, answers its OWN descriptor
/// ([`unnameable_mirror_descriptor`], `L<host>$$Lambda/0x<id>;`, what HotSpot's
/// `toMethodDescriptorString` spells for a hidden class) instead of `None`, so
/// an exact VarHandle over a proxy-typed coordinate or value is judged as
/// HotSpot judges it rather than skipped. Any other unnameable mirror is still
/// `None`.
pub fn strict_mirror_descriptor(ctx: &dyn NativeContext, mirror: ObjectRef) -> Option<String> {
    let Some(name) = resolve_class_name_robust(ctx, mirror) else {
        return unnameable_mirror_descriptor(ctx, mirror);
    };
    if name.is_empty() {
        return None;
    }
    Some(class_name_to_descriptor(&name.replace('.', "/")).into_owned())
}

/// The access mode type (handle excluded) of access type `access_type` for an
/// EXACT `IndirectVarHandle`, from its own `value` and `coordinates`: what
/// HotSpot's `accessModeTypeUncached` answers. `None` for any other handle, a
/// non-exact one, the switch off, or a mirror that does not name its class.
pub fn indirect_vh_exact_access_descriptor(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    access_type: i32,
) -> Option<String> {
    if !vh_indirect_exact_enabled() || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    if !matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0) {
        return None;
    }
    let (coord_descs, value_desc) = indirect_vh_shape(ctx, vh)?;
    Some(vh_access_mode_descriptor(
        access_type,
        &coord_descs,
        &value_desc,
    ))
}

/// An `IndirectVarHandle`'s own coordinate descriptors and variable
/// descriptor, from its `coordinates` / `value` fields (what
/// `accessModeTypeUncached` reads on HotSpot); `None` when either is not a
/// readable class mirror. Reads only.
pub fn indirect_vh_shape(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(Vec<String>, String)> {
    let value = indirect_vh_field(ctx, vh, "value")?;
    let value_desc = strict_mirror_descriptor(&*ctx, value)?;
    let coords = indirect_vh_field(ctx, vh, "coordinates")?;
    if !matches!(ctx.heap_kind_of(coords), ObjectKind::Array) {
        return None;
    }
    let n = ctx.array_length(coords);
    let mut coord_descs: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        let Value::Object(Some(mirror)) = ctx.get_array_element(coords, i) else {
            return None;
        };
        coord_descs.push(strict_mirror_descriptor(&*ctx, mirror)?);
    }
    Some((coord_descs, value_desc))
}

/// The number of trailing VALUE parameters access mode `ordinal` takes: `get*`
/// none, `set*` / `getAndUpdate` one, `compareAnd*` two.
pub fn vh_access_mode_value_count(ordinal: usize) -> usize {
    match VH_ACCESS_MODES
        .get(ordinal)
        .and_then(|(_, method)| vh_access_type_of_method(method))
    {
        Some(1) | Some(4) => 1,
        Some(2) | Some(3) => 2,
        _ => 0,
    }
}

/// `MethodType.canConvert(site, declared)` for one VALUE parameter, `class`
/// being its call-site [`vh_site`](cratonvm_native_api::poly_call_site::vh_site)
/// class. `None` (not judged) when nothing was armed, and for a reference site
/// into a reference parameter (the cast `invokeWithArguments` performs).
pub fn vh_indirect_site_admits(declared: &str, class: u8) -> Option<bool> {
    use cratonvm_native_api::poly_call_site::vh_site;
    if class == vh_site::UNKNOWN {
        return None;
    }
    match declared.as_bytes() {
        [p] if vh_site::is_primitive(*p) => vh_site_admits_primitive(class, *p),
        _ if vh_site::is_primitive(class) => {
            let prim = char::from(class).to_string();
            Some(
                wrapper_desc_for_primitive(&prim)
                    .is_some_and(|wrapper| reference_accepts_wrapper(declared, wrapper)),
            )
        }
        _ => None,
    }
}

/// The `WrongMethodTypeException` `asType` raises for a parameter of an
/// `IndirectVarHandle` access whose static call-site type does not convert
/// ([`vh_indirect_site_admits`]), or `None`. `site` is the parameter-site
/// word (`poly_call_site::vh_param_site_classes`): one class byte per
/// call-site parameter, the LAST parameter in the low byte. The values of an
/// access mode trail its coordinates, so the low byte(s) are the values and
/// the bytes above them the coordinates, last coordinate first (round 15
/// wave 1 hand-back judged the last coordinate of a one-value mode,
/// `r15w1-vh-indirect-coordinate-narrowing-FIXED-20260930.md`; round 15 wave 3,
/// lane vh3, VH1-3, every coordinate the word covers, read modes included).
/// A door that armed only the old 16-bit cell leaves the upper bytes
/// `UNKNOWN`, which are not judged. Coordinates are judged under their own
/// switch, and `CRATONVM_VH_INDIRECT_SITE_ALL_COORDS=0` judges only the two
/// low positions again (the wave-1 reach). `params` is the mode handle's
/// declared `(VarHandle, coordinates..., values...)`, one for one with
/// `args`; the handle itself (position 0) is never a call-site parameter.
pub fn vh_indirect_value_site_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    params: &[String],
    ordinal: usize,
    site: u64,
) -> Option<MethodCallFailed> {
    use cratonvm_native_api::poly_call_site::{
        vh_param_site_class_at, vh_site, VH_PARAM_SITE_SLOTS,
    };
    if site == 0 || params.len() != args.len() {
        return None;
    }
    let values = vh_access_mode_value_count(ordinal);
    let reach = if vh_indirect_site_all_coords_enabled() {
        VH_PARAM_SITE_SLOTS
    } else {
        2
    };
    // `k` counts back from the last parameter; the earliest one first, the
    // order `asType` walks them.
    for k in (0..reach.min(params.len())).rev() {
        let Some(pos) = params.len().checked_sub(1 + k) else {
            continue;
        };
        if pos == 0 {
            continue;
        }
        let judged = if k < values {
            vh_indirect_site_wmte_enabled()
        } else {
            vh_indirect_site_coord_wmte_enabled()
        };
        if !judged {
            continue;
        }
        let class = vh_param_site_class_at(site, k);
        let declared = params[pos].as_str();
        // A reference class judges a reference only (the other value
        // natives' rule, `vh_adapt_value`): a stand-in site whose value
        // arrived primitive is not this parameter's static type.
        if !vh_site::is_primitive(class) && !matches!(args[pos], Value::Object(_)) {
            continue;
        }
        if vh_indirect_site_admits(declared, class) != Some(false) {
            continue;
        }
        // The handle's shape goes with the refusal, for the door's
        // two-type message (wave 53, L4; `vh_astype_refused`).
        let value = args[pos];
        let raise = |ctx: &mut dyn NativeContext| {
            if vh_site::is_primitive(class) {
                let to = descriptor_to_class_name(declared).class_text(&*ctx);
                crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/invoke/WrongMethodTypeException",
                    &format!("cannot convert {} to {to}", vh_prim_java_name(class)),
                )
            } else {
                // A reference site refused only into a primitive parameter.
                let p = declared.as_bytes().first().copied().unwrap_or(b'I');
                vh_value_wmte(ctx, value, p)
            }
        };
        return Some(match args[0] {
            Value::Object(Some(vh)) => vh_astype_refused(ctx, vh, raise),
            _ => raise(ctx),
        });
    }
    None
}

/// F7-1: the arguments after the handle, each already of the mode handle's
/// declared parameter type (a primitive widened as `asType` widens it), or
/// `None` when any of them needs a conversion the `invokeWithArguments` door
/// performs (a box, an unbox, a reference cast that fails) or is an array
/// (the varargs repair's territory). References are returned as passed; the
/// caller re-reads them from their pins.
pub fn vh_indirect_typed_args(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    params: &[String],
) -> Option<Vec<Value>> {
    if args.is_empty() || args.len() != params.len() {
        return None;
    }
    let mut out = Vec::with_capacity(args.len() - 1);
    for (value, declared) in args.iter().zip(params.iter()).skip(1) {
        let declared = declared.as_str();
        let converted = match declared.as_bytes() {
            [b'Z' | b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D'] => {
                let (converted, desc) = vh_indirect_box_input(*value, Some(declared))?;
                if desc != declared {
                    return None;
                }
                converted
            }
            [b'L', ..] => match value {
                Value::Object(_)
                    if reference_arg_admitted(ctx, *value, declared) != Some(false) =>
                {
                    *value                }
                _ => return None,            },
            _ => return None,
        };
        out.push(converted);    }
    Some(out)
}

// ---------------------------------------------------------------------------
// SBR5 lane avh: `MemoryLayout.arrayElementVarHandle` over the JDK's own bodies
// ---------------------------------------------------------------------------
//
// `AbstractLayout.arrayElementVarHandle(elements)` is
// `MethodHandles.collectCoordinates(varHandle(elements), 1, scaleHandle())`: an
// `IndirectVarHandle` whose index coordinate runs `MemoryLayout.scale(offset, index)`
// (`offset + index * byteSize`, a `NegativeArraySize`-free check of both operands and
// exact arithmetic) before the segment handle's own access. Through the composed
// `MethodHandle` that is a dozen `LambdaForm` entries per access (3-5 us each here), 95 us
// per get; the shortcut below makes the access the segment handle's own at the scaled
// offset (about 2 us), after the checks the composed road shares.
//
// Soundness is by CONSTRUCTION, not by shape guessing: the handle's captured filter is
// compared with the filter the JDK itself builds for the same layout
// (`collectCoordinates(target, 1, layout.scaleHandle())`, called once per handle and
// collection epoch): the same `BoundMethodHandle` species over the same `LambdaForm` and the
// same leaf arguments (the checked-exception wrapper `adaptForCheckedExceptions` puts
// around any handle it cannot read the exceptions of). Any difference (a user's own
// coordinate filter, an exact handle, a negative or overflowing scale) declines to the
// composed road, which raises what HotSpot raises.

pub const ARRAY_ELEM_MEMO_SLOTS: usize = 64;

#[derive(Clone)]
pub struct ArrayElemLine {
    pub vm: usize,
    pub epoch: u64,
    pub addr: usize,
    pub filled: bool,
    pub plan: Option<(ObjectRef, i64)>,
    /// The mode handle's declared parameters of the plain `get` / `set` modes, filed by the
    /// first access that went through the composed road's screens ([`vh_array_element_note`]).
    pub get_params: Option<Arc<[String]>>,
    pub set_params: Option<Arc<[String]>>,
    /// The class of a segment that passed the full `MemorySegment` admission on such an access.
    pub seg_class: Option<ClassId>,
}

impl ArrayElemLine {
    pub fn empty() -> Self {
        ArrayElemLine {
            vm: 0,
            epoch: 0,
            addr: 0,
            filled: false,
            plan: None,
            get_params: None,
            set_params: None,
            seg_class: None,
        }
    }
}

thread_local! {
    /// Per-thread verdicts of [`vh_array_element_verify`], keyed by the indirect handle's
    /// address, valid for one collection epoch under one VM (the argument of
    /// [`REAL_HANDLE_MEMO`]: nothing moves or is freed within an epoch, and the handle's
    /// `target` / `handleFactory` are final fields).
    pub static ARRAY_ELEM_MEMO: std::cell::RefCell<Vec<ArrayElemLine>> =
        std::cell::RefCell::new(
            (0..ARRAY_ELEM_MEMO_SLOTS).map(|_| ArrayElemLine::empty()).collect(),
        );
}

pub const BMH_SPECIES_PREFIX: &str = "java/lang/invoke/BoundMethodHandle$Species_";

/// The first object under `h` (a `BoundMethodHandle` species tree) that is a
/// `jdk.internal.foreign.layout` layout, searching reference arguments.
pub fn bmh_find_layout(ctx: &mut dyn NativeContext, h: ObjectRef, depth: u32) -> Option<ObjectRef> {
    if depth > 6 {
        return None;
    }
    let cid = ctx.class_id_of_object(h);
    let name = ctx.class_name_arc_of_id(cid)?;
    if name.starts_with("jdk/internal/foreign/layout/") {
        return Some(h);
    }
    let letters = name.strip_prefix(BMH_SPECIES_PREFIX)?;
    for (i, ch) in letters.chars().enumerate() {
        if ch != 'L' {
            continue;
        }
        if let Value::Object(Some(o)) = ctx.get_field_by_name(h, &format!("argL{i}")) {
            if let Some(found) = bmh_find_layout(ctx, o, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Two `BoundMethodHandle` trees that are the same handle built twice: the same species,
/// `form` and `type`, and every argument identical (a nested handle: the same again).
pub fn bmh_tree_equal(ctx: &mut dyn NativeContext, a: ObjectRef, b: ObjectRef, depth: u32) -> bool {
    if a == b {
        return true;
    }
    if depth > 6 {
        return false;
    }
    let ca = ctx.class_id_of_object(a);
    if ca != ctx.class_id_of_object(b) {
        return false;
    }
    let Some(name) = ctx.class_name_arc_of_id(ca) else {
        return false;
    };
    let Some(letters) = name.strip_prefix(BMH_SPECIES_PREFIX) else {
        return false;
    };
    for field in ["form", "type"] {
        if ctx.get_field_by_name(a, field) != ctx.get_field_by_name(b, field) {
            return false;
        }
    }
    for (i, ch) in letters.chars().enumerate() {
        let field = format!("arg{ch}{i}");
        let (va, vb) = (
            ctx.get_field_by_name(a, &field),
            ctx.get_field_by_name(b, &field),
        );
        match (va, vb) {
            (Value::Object(Some(x)), Value::Object(Some(y))) => {
                if x != y && !bmh_tree_equal(ctx, x, y, depth + 1) {
                    return false;
                }
            }
            (x, y) => {
                if x != y {
                    return false;
                }
            }
        }
    }
    true
}

/// The captured variable `name` (`arg$N`) of a lambda: a lambda proxy this VM mints holds its
/// captures in the heap slots `0..n` in factory-argument order (no declared fields), a
/// spun hidden class has the fields.
pub fn lambda_capture(ctx: &mut dyn NativeContext, lambda: ObjectRef, name: &str) -> Value {
    match crate::lang_class::lambda_capture_slot(&*ctx, lambda, name) {
        Some(slot) => ctx.get_field(lambda, slot),
        None => ctx.get_field_by_name(lambda, name),
    }
}

/// Is `vh` the JDK's `arrayElementVarHandle` of a layout over a real `SegmentVarHandle`?
/// `(segment handle, byteSize)`, or `None`. Runs Java (a GC point): `vh` is re-read from
/// `vh_pin`, and the answer's handle is read last.
pub fn vh_array_element_verify(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
) -> Option<(ObjectRef, i64)> {
    vh_array_element_verify_why(ctx, vh_pin, vh).ok()
}

pub fn vh_array_element_verify_why(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
) -> Result<(ObjectRef, i64), &'static str> {
    let vh_now = ctx.read_native_pin(vh_pin, vh);
    let mut scope = NativeHandleScope::new(ctx);
    let vh_h = scope.root(vh_now);
    // An exact handle judges its call site's types itself.
    if !matches!(scope.get_field_by_name(vh_now, "exact"), Value::Int(0)) {
        return Err("exact");
    }
    let factory = indirect_vh_field(&mut *scope, vh_now, "handleFactory").ok_or("no factory")?;
    // A lambda proxy this VM mints has no loaded-class name; the captured arguments and the
    // structural comparison below are what decide.
    let factory_cid = scope.class_id_of_object(factory);
    if let Some(fname) = scope.class_name_arc_of_id(factory_cid) {
        if !fname.starts_with("java/lang/invoke/VarHandles$$Lambda") {
            return Err("factory is not a VarHandles lambda");
        }
    }
    if lambda_capture(&mut *scope, factory, "arg$1") != Value::Int(1) {
        return Err("arg$1 is not 1");
    }
    let Value::Object(Some(filter)) = lambda_capture(&mut *scope, factory, "arg$2") else {
        return Err("no arg$2");
    };
    let target = indirect_vh_field(&mut *scope, vh_now, "target").ok_or("no target")?;
    if !is_segment_var_handle(&mut *scope, target) {
        return Err("target is not a SegmentVarHandle");
    }
    let filter_h = scope.root(filter);
    let target_h = scope.root(target);
    let layout = bmh_find_layout(&mut *scope, filter, 0).ok_or("no layout in the filter")?;
    let layout_h = scope.root(layout);
    let stride = match scope.get_field_by_name(layout, "byteSize") {
        Value::Long(v) if v >= 0 => v,
        _ => return Err("byteSize"),
    };
    // The JDK's own filter for this layout, built the way `arrayElementVarHandle` builds it.
    let layout_now = scope.get(&layout_h);
    let Ok(Some(Value::Object(Some(scale)))) = scope.invoke_virtual(
        layout_now,
        "scaleHandle",
        "()Ljava/lang/invoke/MethodHandle;",
        &[],
    ) else {
        return Err("scaleHandle");
    };
    let scale_h = scope.root(scale);
    let (target_now, scale_now) = (scope.get(&target_h), scope.get(&scale_h));
    let Ok(Some(Value::Object(Some(reference)))) = scope.invoke_static(
        "java/lang/invoke/MethodHandles",
        "collectCoordinates",
        "(Ljava/lang/invoke/VarHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/VarHandle;",
        &[Value::Object(Some(target_now)), Value::Int(1), Value::Object(Some(scale_now))],
    ) else {
        return Err("collectCoordinates");
    };
    let reference_factory =
        indirect_vh_field(&mut *scope, reference, "handleFactory").ok_or("no reference factory")?;
    let Value::Object(Some(reference_filter)) =
        lambda_capture(&mut *scope, reference_factory, "arg$2")
    else {
        return Err("no reference arg$2");
    };
    let _ = vh_h;
    let filter = scope.get(&filter_h);
    if !bmh_tree_equal(&mut *scope, filter, reference_filter, 0) {
        return Err("filter differs from the JDK's");
    }
    Ok((scope.get(&target_h), stride))
}

/// The memoised verdict of [`vh_array_element_verify`] for `vh` in this collection epoch:
/// `Some(verdict)` on a hit; `None` on a miss, after FILING the verdict (the verify ran
/// Java, so the caller's arguments may have moved and it declines this one access).
pub fn vh_array_element_plan_hit(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
) -> Option<Option<(ObjectRef, i64)>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let vh_now = ctx.read_native_pin(vh_pin, vh);
    let epoch = ctx.gc_address_epoch();
    let addr = vh_now.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    let hit = ARRAY_ELEM_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        (line.filled && line.vm == vm && line.epoch == epoch && line.addr == addr)
            .then_some(line.plan)
    });
    if hit.is_some() {
        return hit;
    }
    let plan = vh_array_element_verify(ctx, vh_pin, vh);
    // Keyed by where the handle is NOW, in the epoch it is in NOW.
    let vh_after = ctx.read_native_pin(vh_pin, vh);
    let epoch = ctx.gc_address_epoch();
    let addr = vh_after.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    ARRAY_ELEM_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = ArrayElemLine {
            vm,
            epoch,
            addr,
            filled: true,
            plan,
            ..ArrayElemLine::empty()
        };
    });
    None
}

/// Files what a screened access learned about the (already verified) array-element handle
/// at `vh`: the mode handle's declared parameters for `ordinal`'s kind (plain `get` or
/// `set`) and the class of the segment that passed the `MemorySegment` admission, so the
/// next access of the same shape is served by [`vh_array_element_early`] without them.
pub fn vh_array_element_note(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    is_get: bool,
    params: &Arc<[String]>,
    seg_class: ClassId,
) {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return;
    }
    let epoch = ctx.gc_address_epoch();
    let addr = vh.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    ARRAY_ELEM_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let line = &mut memo[slot];
        if line.filled
            && line.vm == vm
            && line.epoch == epoch
            && line.addr == addr
            && line.plan.is_some()
        {
            if is_get {
                line.get_params = Some(Arc::clone(params));
            } else {
                line.set_params = Some(Arc::clone(params));
            }
            line.seg_class = Some(seg_class);
        }
    });
}

/// The first thing a plain `get` / `set` of an array-element handle does, before the mode
/// handle is composed or its parameters rendered: when this handle's verdict and the
/// screens of an earlier access of the same shape are filed (this epoch), re-judge only
/// what varies (the call site's value types, the coordinates' and the value's variants, the
/// segment's class) and make the access. `None` declines to the composed road, which
/// screens everything itself.
pub fn vh_array_element_early(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    ordinal: usize,
    site: u64,
) -> Option<MethodCallResult> {
    let method = VH_ACCESS_MODES[ordinal].1;
    let is_get = matches!(method, "get" | "getVolatile" | "getOpaque" | "getAcquire");
    let is_set = matches!(method, "set" | "setVolatile" | "setOpaque" | "setRelease");
    if !((is_get && args.len() == 4) || (is_set && args.len() == 5)) {
        return None;
    }
    let (Value::Object(Some(seg)), Value::Long(base), Value::Long(index)) =
        (args[1], args[2], args[3])
    else {
        return None;
    };
    if base < 0 || index < 0 {
        return None;
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let epoch = ctx.gc_address_epoch();
    let addr = vh.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    let (target, stride, params, seg_class) = ARRAY_ELEM_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if !(line.filled && line.vm == vm && line.epoch == epoch && line.addr == addr) {
            return None;
        }
        let (target, stride) = line.plan?;
        let params = if is_get {
            line.get_params.clone()
        } else {
            line.set_params.clone()
        }?;
        Some((target, stride, params, line.seg_class?))
    })?;
    if ctx.class_id_of_object(seg) != seg_class || params.len() != args.len() {
        return None;
    }
    // The call site's static types, as the composed road judges them.
    if site != 0 {
        if let Some(refusal) = vh_indirect_value_site_refusal(ctx, args, &params, ordinal, site) {
            return Some(Err(refusal));
        }
    }
    // The value is the declared type exactly (the segment, admitted above, stands as `null`
    // for the typed screen so that it does not repeat the class walk).
    if is_set {
        let mut screened: [Value; 5] = [Value::Object(None); 5];
        screened[2] = args[2];
        screened[3] = args[3];
        screened[4] = args[4];
        vh_indirect_typed_args(ctx, &screened, &params)?;
    }
    let offset = base.checked_add(index.checked_mul(stride)?)?;
    Some(if is_get {
        varhandle_get_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
            ],
            method,
        )
    } else {
        varhandle_set_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
                args[4],
            ],
            method,
        )
    })
}

/// The access `rest` = `(segment, base, index[, value])` of the `arrayElementVarHandle`
/// whose plain access mode is `ordinal`, made on the segment handle at `base + index *
/// byteSize`; `None` declines to the composed road (another access mode, another handle
/// shape, a negative or overflowing scale, a verdict not filed yet).
pub fn vh_array_element_forward(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
    rest: &[Value],
    params: &Arc<[String]>,
) -> Option<MethodCallResult> {
    let method = VH_ACCESS_MODES[ordinal].1;
    let is_get = matches!(method, "get" | "getVolatile" | "getOpaque" | "getAcquire");
    let is_set = matches!(method, "set" | "setVolatile" | "setOpaque" | "setRelease");
    if !((is_get && rest.len() == 3) || (is_set && rest.len() == 4)) {
        return None;
    }
    let (Value::Object(Some(seg)), Value::Long(base), Value::Long(index)) =
        (rest[0], rest[1], rest[2])
    else {
        return None;
    };
    if base < 0 || index < 0 {
        return None;
    }
    let (target, stride) = vh_array_element_plan_hit(ctx, vh_pin, vh)??;
    let offset = base.checked_add(index.checked_mul(stride)?)?;
    // The screens of the composed road passed for this access: the next access of this
    // shape skips them ([`vh_array_element_early`]).
    let seg_class = ctx.class_id_of_object(seg);
    let vh_now = ctx.read_native_pin(vh_pin, vh);
    vh_array_element_note(ctx, vh_now, is_get, params, seg_class);
    Some(if is_get {
        varhandle_get_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
            ],
            method,
        )
    } else {
        varhandle_set_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
                rest[3],
            ],
            method,
        )
    })
}

/// A REAL handle's own `type()` as a descriptor, or `None`.
pub fn mh_real_declared_descriptor(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return None;
    };
    crate::link_to_method_descriptor(&*ctx, mt)}

/// F7-1: the composed mode handle is one of this file's carriers (the
/// `invokeExact` door's model) and not a varargs collector.
pub fn vh_indirect_typed_admits_handle(ctx: &mut dyn NativeContext, mh: ObjectRef) -> bool {
    !mh_is_real_handle(&*ctx, mh) && mh_marking_bits(&*ctx, mh) & MH_MARK_VARARGS == 0
}


/// `CRATONVM_VH_ARITY_WMTE=0` — restore the pre-wave-19 behaviour, in which a
/// `VarHandle` access whose argument COUNT does not match the access mode's
/// type ran anyway (a surplus trailing argument was dropped, a missing one
/// read as `0`/`null`). Default on. Read only on the refusal path, so it costs
/// an exact-arity call nothing.
pub fn vh_arity_wmte_enabled() -> bool {
    !matches!(
        None::<String>.as_deref(),
        Some("0")
    )
}

/// Interpreter round i1 wave 47, lane L4
/// (`interpreter-L4-proposal-one-varhandle-row-read-per-access-FIXED-20261011`): the `get*`, `set*`
/// and `compareAndSet` / `weakCompareAndSet*` natives read their handle's
/// side-table row ONCE, at the top ([`vh_prologue_row`]), and hand it to the
/// prologue's checks and the body, which each read it again before; and an
/// INSTANCE or STATIC row skips the two FFM predicates
/// (`is_segment_var_handle`, a class-name fetch, and `p67_segment_vh_shape`,
/// a second memo probe), which can only answer "no" for it: such a row is
/// written only on a fresh synthetic `VarHandle` by `alloc_instance_var_handle`
/// / `alloc_static_var_handle` (copied by `varhandle_with_exactness`), none of
/// which is a `SegmentVarHandle` or registers a layout shape, and both tables
/// drop a dead handle's lock key together (`forget_var_handle_meta_keys`).
/// The row holds no object reference, so it stays valid across the GC point
/// of the value adaptation. `false` restores the per-step reads. A
/// compile-time switch: one constant branch, no per-VM state.
pub const VARHANDLE_ONE_ROW_READ: bool = true;

/// The row an access native's prologue read: `Some(row)` (itself `None` for
/// a handle with no row) when [`VARHANDLE_ONE_ROW_READ`] is on, `None` when
/// every step reads its own.
pub type VhPrologueRow = Option<Option<Arc<VarHandleMeta>>>;

/// Read the handle's row once for an access native (`None` with the switch
/// off, or with no handle to ask).
#[inline]
pub fn vh_prologue_row(ctx: &mut dyn NativeContext, args: &[Value]) -> VhPrologueRow {
    if !VARHANDLE_ONE_ROW_READ {
        return None;
    }
    match args.first() {
        Some(Value::Object(Some(vh))) => Some(vh_meta_get(ctx, *vh)),
        _ => None,
    }
}

/// `vh`'s row: the prologue's when it read one, else a fresh read.
#[inline]
pub fn vh_row_or_read(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    row: &VhPrologueRow,
) -> Option<Arc<VarHandleMeta>> {
    match row {
        Some(meta) => meta.clone(),
        None => vh_meta_get(ctx, vh),
    }
}

/// The prologue read an INSTANCE or STATIC row, for which the FFM predicates
/// can only answer "no" (see [`VARHANDLE_ONE_ROW_READ`]).
#[inline]
pub fn vh_row_is_field(row: &VhPrologueRow) -> bool {
    matches!(row, Some(Some(m)) if m.kind == VH_KIND_INSTANCE || m.kind == VH_KIND_STATIC)
}

/// `vh`'s FFM layout shape, not asked for an INSTANCE or STATIC row, which
/// cannot have one (see [`VARHANDLE_ONE_ROW_READ`]).
#[inline]
pub fn vh_layout_shape_unless_field(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
    row: &VhPrologueRow,
) -> Option<SegmentVhShape> {
    if vh_row_is_field(row) {
        return None;
    }
    p67_segment_vh_shape(ctx, vh)
}

/// The `asType` ARITY check every access mode owes its call site, before any
/// value is looked at (round 11 wave 19, lane vh;
/// r11w18-orch-varhandle-set-accepts-a-surplus-reference-argument).
///
/// # What was wrong
///
/// javac does not check the arity of a signature-polymorphic call, so
/// `A.set(h, arr, "s")` on an `int[]` field handle compiles to
/// `VarHandle.set:(LH;[ILjava/lang/String;)V`. HotSpot's `asType` from the
/// access mode's type `(H,int[])void` to that one fails on the parameter
/// COUNT: `WrongMethodTypeException`, field untouched. Every native here read
/// the values it NEEDED by position and never looked at the rest, so the
/// surplus `String` was dropped and `arr` stored — in every tier, `--nojit`
/// included (`R11W18VhArrayValueArity`: `arity.set.String none=20000`). A
/// surplus PRIMITIVE was refused only by accident (the reference-variable
/// boxing pass judged the trailing `int` as the value: WMTE for the wrong
/// reason), and a MISSING value read as `Int(0)` and stored it.
///
/// # The rule
///
/// `args` is `[handle, coordinates..., values...]`, one `Value` per Java
/// parameter. The access mode fixes the value count (`access_type` is the
/// `VarHandle.AccessType` ordinal `vh_access_mode_descriptor` switches on:
/// `GET` 0, `SET` 1, `COMPARE_AND_SET` 2, `COMPARE_AND_EXCHANGE` 3,
/// `GET_AND_UPDATE` 4), the handle's kind fixes the coordinate count, and any
/// other total is WMTE. It runs after the read-only and unsupported-mode UOE
/// checks (HotSpot's `getMethodHandle(mode)` raises those before `asType`)
/// and before the null-coordinate NPE and the value adaptation (`asType`
/// precedes the access).
///
/// # Only what is known
///
/// The coordinate count is taken from an AUTHORITATIVE source only: the
/// side-table kind, or a real-JDK array-element handle's own class. A handle
/// with no side-table entry that is not a real array handle, and every FFM
/// layout/segment handle (whose coordinate lists are not modelled here), is
/// not judged — today's behaviour, not a guess.
///
/// # Why it is free
///
/// One `vh_meta_get` (the thread-local memo hit every native here already
/// pays two or three of) and an integer compare. The FFM screens, the
/// message and the kill switch are read on the refusal path only.
pub fn vh_check_arity(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    access_type: i32,
    mode: &'static str,
) -> Result<Option<i32>, MethodCallFailed> {
    vh_check_arity_row(ctx, args, access_type, mode, &None)
}

/// [`vh_check_arity`] over the row the access native's prologue already read
/// ([`vh_prologue_row`]; `None` reads it here, as before wave 47).
pub fn vh_check_arity_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    access_type: i32,
    mode: &'static str,
    row: &VhPrologueRow,
) -> Result<Option<i32>, MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // No handle: `obj_arg` reports it, as it does today.
        _ => return Ok(None),
    };
    let values = match access_type {
        0 => 0,
        2 | 3 => 2,
        _ => 1,
    };
    let given = args.len() - 1;
    let meta = vh_row_or_read(ctx, this, row);
    let kind = meta.as_deref().map(|m| m.kind);
    let coords = match kind {
        Some(VH_KIND_STATIC) => 0,
        Some(VH_KIND_INSTANCE) => 1,
        Some(VH_KIND_ARRAY) => 2,
        Some(
            VH_KIND_BYTE_VIEW_LE
            | VH_KIND_BYTE_VIEW_BE
            | VH_KIND_BYTE_BUFFER_VIEW_LE
            | VH_KIND_BYTE_BUFFER_VIEW_BE,
        ) => {
            if let Some(m) = meta.as_deref() {
                vh_view_mode_refusal(ctx, args, m, mode)?;
            }
            2
        }
        // A kind this does not model (the FFM layout kind): not judged.
        Some(_) => return Ok(kind),
        None => {
            // Exact for an array handle: no class-name read on the hot path.
            if given == 2 + values {
                return Ok(None);
            }
            if real_array_var_handle_descriptors(ctx, this).is_none() {
                return Ok(None);
            }
            2
        }
    };
    if given == coords + values {
        return Ok(kind);
    }
    // The refusal path. An FFM handle is left alone even if a side-table row
    // answered for its identity hash: its natives decide it before the meta.
    if p67_segment_vh_shape(ctx, this).is_some() || is_segment_var_handle(ctx, this) {
        return Ok(kind);
    }
    if !vh_arity_wmte_enabled() {
        return Ok(kind);
    }
    let (_kind, value_desc, coord_descs, _exact) = vh_value_and_coordinate_descriptors(ctx, this);
    let desc = vh_access_mode_descriptor(access_type, &coord_descs, &value_desc);
    // `MethodType.toString` of the handle's invoker type, as HotSpot's
    // `asType` message spells it: the `VarHandle` leads the parameters.
    let shown = match method_type_display_in(&*ctx, &desc) {
        Some(s) if s.starts_with("()") => format!("(VarHandle{}", &s[1..]),
        Some(s) if s.starts_with('(') => format!("(VarHandle,{}", &s[1..]),
        Some(s) => s,
        None => desc,
    };
    // The door that knows the call site's descriptor rewrites this text as
    // HotSpot's `cannot convert MethodHandle(<own>) to <site>` (wave 53, L4).
    let text = format!(
        "cannot convert MethodHandle{shown}: the call site passes {given} argument(s) after the VarHandle, the access mode takes {}",
        coords + values
    );
    Err(vh_astype_refused(ctx, this, |ctx| {
        crate::phases_early::throw_jca_exc(ctx, "java/lang/invoke/WrongMethodTypeException", &text)
    }))
}

pub fn varhandle_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_mode(ctx, args, "get")
}

/// [`varhandle_get`] for the access mode `mode` (`get`, `getVolatile`,
/// `getOpaque`, `getAcquire`): the byte views admit fewer modes than a field.
pub fn varhandle_get_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    // Round 15 wave 3 (lane vh3; VH1-3): the parameter-site word, TAKEN first
    // like the store modes' cell, so a door's word never outlives this access.
    // Its coordinates are what an `IndirectVarHandle`'s `asType` judges.
    let site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    let call_site = cratonvm_native_api::poly_call_site::take();
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    // SBR3 lane ffmperf: a planned real `SegmentVarHandle` (it has no row) over
    // a real native segment, answered without the generic prologue; anything
    // the lean path cannot prove declines to the road below.
    if mode == "get" && matches!(row, Some(None)) {
        // r23 wave 2c, lane L8: a call site whose return is a primitive takes
        // the lean road only when it is the carrier itself (any conversion is
        // `asType`'s, below); a reference-class site declines.
        let ret = cratonvm_native_api::poly_call_site::vh_param_site_return(site);
        if ret == 0 || ret >= b'A' {
            if let Some(Some(value)) = segment_vh_lean_native(ctx, args, false, ret) {
                return Ok(Some(value));
            }
        }
    }
    // A combinator's handle is its adapter's to check (round 14 wave 7).
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, site, &row) {
        return served;
    }
    if let (Some(Some(meta)), Some(call_site)) = (&row, call_site.as_deref()) {
        let expected = if let Some((elem, _)) = byte_view_kind(Some(meta)) {
            vh_view_mode_refusal(ctx, args, meta, mode)?;
            Some(format!("([BI){}", elem as char))
        } else if meta.kind == VH_KIND_INSTANCE && !meta.class_name.is_empty() {
            Some(format!(
                "({}){}",
                class_name_to_descriptor(&meta.class_name),
                meta.field_desc
            ))
        } else if meta.kind == VH_KIND_STATIC {
            Some(format!("(){}", meta.field_desc))
        } else {
            None
        };
        if let Some(expected) = expected {
            // Judge the complete access type before coordinate faults. Carrier
            // mismatches that the thin helper declines must not silently narrow
            // or reinterpret on this shared native fallback.
            if method_type_is_convertible_to(&expected, call_site) == Some(false) {
                let old = vh_invoker_descriptor(&expected).unwrap();
                let new = vh_invoker_descriptor(call_site).unwrap();
                if let (Some(old), Some(new)) = (
                    method_type_display_in(&*ctx, &old),
                    method_type_display_in(&*ctx, &new),
                ) {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/lang/invoke/WrongMethodTypeException",
                        &format!("cannot convert MethodHandle{old} to {new}"),
                    ));
                }
            }
        }
    }
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    let own = vh_return_site_check(ctx, args, site, &row)?;
    let result = varhandle_get_mode_access(ctx, args, mode, site, &row);
    // Wave 59 (lane L4): a wrapper-typed variable's `null` at its own
    // primitive's site.
    vh_own_wrapper_null(ctx, own, result)
}

/// [`varhandle_get_mode`] after its prologue and return-site check: the
/// access itself.
pub fn varhandle_get_mode_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
    site: u64,
    row: &VhPrologueRow,
) -> MethodCallResult {
    // Round 15 wave 3 (lane vh4; VH3-2): the index coordinate as `asType`
    // converts it, so every decode below reads the REAL index.
    let coordinate = vh_index_coordinate_args(ctx, args, 0, site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): the array coordinate's cast.
    vh_array_coordinate_cast(ctx, args, 0, mode)?;
    vh_check_arity_row(ctx, args, 0, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    // An instance or static field row cannot be an FFM handle (wave 47).
    if !vh_row_is_field(&row) {
        // `SegmentVarHandle` (JEP 454 FFM API): a distinct real class with its own
        // (real) fields, no synthetic VH_KIND entry. Must be checked before the
        // meta/array/byte-view paths below, which don't apply to it.
        if is_segment_var_handle(ctx, this) {
            if let Some(refusal) = segment_vh_ordered_mode_refusal(ctx, this, mode) {
                return Err(refusal);
            }
            if let Some(result) = segment_vh_get(ctx, this, args) {
                if result.is_err() {
                    if let Some(jdk) = segment_vh_raise_through_jdk(ctx, this, args, mode) {
                        return jdk;
                    }
                }
                return result;
            }
        }
        // An FFM LAYOUT handle (`ValueLayout.JAVA_INT.varHandle()`,
        // `layout.varHandle(PathElement...)`) is a synthetic receiver whose meaning
        // lives only in the shape table, so it must be answered before the slot
        // reads below — which would take slot 0 (its endianness flag) for a kind
        // tag and route the access to the instance-field arm, where it silently
        // did nothing.
        if let Some(shape) = p67_segment_vh_shape(ctx, this) {
            // `layout_vh_get`, not `segment_vh_get`: the latter takes the real
            // `SegmentVarHandle` receiver handled above, this one takes the shape.
            // The `set` twin twelve lines below already had it right.
            return layout_vh_get(ctx, shape, args);
        }
    }
    // Round-7 HIGH-2 fix: fetch the side-table meta exactly once and reuse
    // the bound Arc for `kind`/`field_index`/`class_name`/`field_name`/
    // `field_desc`. Previously each helper (`vh_meta_get`, `vh_type_desc`,
    // the by-name resolve fallback) re-locked the table 2–3× per native.
    let meta = vh_row_or_read(ctx, this, &row);
    // byte-array-view (`asLittleEndian.get([BI)J` etc.): MUST be handled BEFORE
    // the array-element fast path, because both have the [vh, array, index]
    // arg shape — but a view reads `width` bytes at a BYTE index, whereas the
    // array path would return a single signed byte.
    if let Some((elem, le)) = byte_view_kind(meta.as_deref()) {
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let index = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let idx = match byte_view_check_index(ctx, arr, index, elem) {
            Ok(i) => i,
            Err(fallback) => {
                let at = Value::Object(Some(arr));
                let view = Some((elem, le, false));
                return Err(vh_index_raise_through_jdk(
                    ctx, this, view, mode, at, index, fallback,
                ));
            }
        };
        let value = byte_view_get(ctx, arr, idx, elem, le);
        // CANONICAL — measured `vh.byteViewInt` = true and `vh.byteViewLong`
        // = true. `byte_view_get` and `byte_view_desc` switch on the same
        // `elem`, so variant and descriptor agree.
        return Ok(Some(box_value_canonical(ctx, value, byte_view_desc(elem))));
    }
    if let Some((elem, le)) = byte_buffer_view_kind(meta.as_deref()) {
        let mut bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Object(None))),
        };
        let index = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let idx = match byte_buffer_view_check_index(ctx, bb, index, elem) {
            Ok(i) => i,
            Err(fallback) => {
                let at = Value::Object(Some(bb));
                let view = Some((elem, le, true));
                return Err(vh_index_raise_through_jdk(
                    ctx, this, view, mode, at, index, fallback,
                ));
            }
        };
        let _window = byte_buffer_view_access_window(ctx, &mut bb)?;
        let value = byte_buffer_view_get(ctx, bb, idx, elem, le).unwrap_or(Value::Object(None));
        // CANONICAL — the `ByteBuffer` twin of the `byte[]` view above; same
        // measurement. Note the `unwrap_or(Value::Object(None))` on the line
        // before: on a failed read the pair is `(desc, Value::Object(None))`,
        // which matches none of the six cached arms, so it falls back to
        // `box_value` and keeps that (pre-existing, separately nominated)
        // shape byte-for-byte rather than caching a null-carrying wrapper.
        return Ok(Some(box_value_canonical(ctx, value, byte_view_desc(elem))));
    }
    // C38: Array-element VarHandle call — detected by args[1] being an array
    // and args[2] being an Int. Handles real-JDK VarHandleLongs$Array and the
    // other primitive-array VarHandle subclasses produced by
    // MethodHandles.arrayElementVarHandle. Returns the raw primitive Value
    // (Long, Int, Float, Double, Reference) — unbox_poly_return / the
    // signature-polymorphic call-site descriptor steers it to the right
    // primitive return slot.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let desc = array_element_desc(ctx, arr);
        let value = ctx.get_array_element(arr, idx);
        // CANONICAL — measured `vh.arrInt` / `arrChar` / `arrBool` / `arrLong`
        // = true. NOTE the contrast this must not be "unified" with:
        // `java.lang.reflect.Array.get` on the SAME `int[]` is FRESH on both
        // VMs (`array.int` = false, and `array.selfid` = false — it is not
        // even identical to itself). Two reads of one array element, two
        // different contracts, because `Array.get` is `Reflection::array_get`
        // -> `create()` while a `VarHandle` gets a `valueOf`-shaped adapter.
        return Ok(Some(box_value_canonical(ctx, value, desc)));
    }
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_INSTANCE => {
            // args[1] is the receiver object directly (signature-polymorphic dispatch)
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, None) {
                return Err(refusal);
            }
            let td = match meta.as_deref() {
                Some(m) => vh_type_desc_from_meta(m),
                None => vh_type_desc(ctx, this),
            };
            if field_idx >= 0 {
                // zt t2/nd: a VarHandle read of `Reference.referent` keeps the
                // referent alive (`slot0_read_keep_alive`).
                let slot = field_idx as usize;
                let raw = ctx.get_field(receiver, slot);
                let raw = crate::unsafe_natives::slot0_read_keep_alive(ctx, receiver, slot, raw);
                let val = crate::lang_class::coerce_reflective_field_value(raw, &td);
                // CANONICAL — measured `vh.fieldInt` / `fieldChar` /
                // `fieldBool` / `fieldLong` / `fieldByte` / `fieldShort` =
                // true, and `vh.fieldBoolTRUE` = true (a `boolean` field read
                // through a VarHandle IS `Boolean.TRUE`, not a look-alike).
                //
                // `ctx.get_field` returns the RAW slot, which for a `long`
                // field can present as a compact `Value::Int` — which is why
                // the read is wrapped in `coerce_reflective_field_value`, the
                // SAME function `Field.get` uses. Without it this arm produced
                // a `Long` wrapper whose slot 0 held raw compact-`Int` bits: a
                // wrong ANSWER, not merely a non-canonical one, and one the
                // helper's variant guard could only downgrade to a fresh box
                // rather than repair.
                //
                // F19-1 §5.5 listed `vh.fieldLong`'s HotSpot verdict as
                // **unknown**. It is now MEASURED on OpenJDK 25.0.3+9
                // (`VhLong.java`, identical under `-Xint`), and the direction
                // is the same as `Field.get`'s — widen FIRST, then box
                // canonically, not the reverse:
                //
                //   vh.fieldLong      = java.lang.Long, value 5,  id true
                //   vh.fieldLongZero  = value 0,                  id true
                //   vh.fieldDouble    = java.lang.Double, 1.5,    id FALSE
                //   vh.getAndSetLong  = old value 5,              id true
                //   field.long        = value 5,                  id true
                //
                // The fix is `coerce_reflective_field_value` at this arm,
                // at the by-name arm below, at `VH_KIND_STATIC` (whose
                // `get_static_field` is equally raw), and at
                // `vh_box_access_result` — which the `getAndSet` row above
                // puts inside the family rather than beside it. All FOUR are
                // applied; three of them would have looked like the whole set.
                // `Double` is NOT an identity row (id false) but IS a value
                // row: a `double` slot carrying raw bits as a `Value::Long`
                // must be REINTERPRETED, which is the arm this file's own
                // `widen_primitive_to_descriptor` would get WRONG — it
                // converts numerically, so `1.5` comes back as `4.609e18`.
                // One rule, one implementation, and it is `lang_class`'s;
                // substituting the local widener here is a silent wrong
                // answer, not a shortcut.
                Ok(Some(box_value_canonical(ctx, val, &td)))
            } else {
                // Resolve by name (reuse the meta Arc we already hold).
                // By name in the field's holder, not the coordinate.
                match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    Some(idx) => {
                        // Cache for next time
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(idx as i32));
                        vh_meta_update_field_index(ctx, this, idx as i32);
                        // Widened through the SAME function as the
                        // `field_idx >= 0` arm above: a VarHandle that
                        // resolved late must not answer differently from one
                        // that resolved early, in the value any more than in
                        // the identity.
                        // zt t2: the same `Reference.referent` keep-alive as
                        // the by-index arm.
                        let raw = ctx.get_field(receiver, idx);
                        let raw =
                            crate::unsafe_natives::slot0_read_keep_alive(ctx, receiver, idx, raw);
                        let val = crate::lang_class::coerce_reflective_field_value(raw, &td);
                        // Reuse already-computed type descriptor. CANONICAL,
                        // for the same measurement as the `field_idx >= 0`
                        // arm twenty lines up — this is the same read after a
                        // by-name resolve, and a VarHandle that happened to
                        // resolve late must not answer with a different
                        // identity than one that resolved early.
                        Ok(Some(box_value_canonical(ctx, val, &td)))
                    }
                    None => Ok(Some(Value::Object(None))),
                }
            }
        }
        VH_KIND_STATIC => {
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            let raw = match vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut [],
                vh_static_init(mode, meta.as_deref()),
            )? {
                Some((cid, sidx)) => ctx.get_static_field(cid, sidx),
                None => return Ok(Some(Value::Object(None))),
            };
            let td = match meta.as_deref() {
                Some(m) => vh_type_desc_from_meta(m),
                None => vh_type_desc(ctx, this),
            };
            // Widen BEFORE boxing, and therefore after `td` — the read above
            // is deliberately named `raw`, because that is what
            // `get_static_field` hands back.
            let val = crate::lang_class::coerce_reflective_field_value(raw, &td);
            // CANONICAL — measured `vh.staticInt` = true. The out-of-bound
            // twin is measured too and is the arm the helper delegates:
            // `vhoob.staticInt1000` = false.
            //
            // `get_static_field` is as raw as `get_field`, so this arm is a
            // member of the widening set — see the block on the
            // `VH_KIND_INSTANCE` arm above for the measurement and for why
            // this file's own `widen_primitive_to_descriptor` is the wrong
            // function for it.
            Ok(Some(box_value_canonical(ctx, val, &td)))
        }
        VH_KIND_ARRAY => {
            // args = [vh, array, index]
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Round 15 wave 3 (lane vh4): checked, and only for an array.
            let array_class = meta.as_deref().map(|m| m.class_name.as_str());
            let Some(idx) = vh_kind_array_fallback_index(ctx, arr, array_class, args)? else {
                return Ok(Some(Value::Object(None)));
            };
            // W8-13: this branch returned the BARE element — the one
            // unboxed return left in `varhandle_get`, and the same defect the
            // RMW modes had. It is normally shadowed by the `vh_array_call`
            // fast path above, which matches whenever args[2] is an `Int`; it
            // is reachable only when the index coordinate is absent or not an
            // `Int`, i.e. exactly the shape a partial fix would leave behind.
            let value = ctx.get_array_element(arr, idx);
            let desc = array_element_desc(ctx, arr);
            // CANONICAL, for the same measurement as the `vh_array_call` fast
            // path above. Switched even though the comment above says this
            // branch is normally shadowed: an arm that is reachable only in
            // the shape a partial fix leaves behind is exactly the arm that
            // must not disagree with its twin.
            Ok(Some(box_value_canonical(ctx, value, desc)))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// VarHandle.set(receiver, value) → void
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver, value] for instance fields.
pub fn varhandle_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_set_mode(ctx, args, "set")
}

/// [`varhandle_set`] for the access mode `mode` (`set`, `setOpaque`,
/// `setRelease`, and `setVolatile` through [`varhandle_set_volatile`]).
pub fn varhandle_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    // FIRST, before any early return: the cell belongs to this call only.
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`). It
    // holds no object reference, so the value adaptation's GC point below
    // leaves it valid.
    let row = vh_prologue_row(ctx, args);
    // SBR3 lane ffmperf: see `varhandle_get_mode`.
    if mode == "set"
        && matches!(row, Some(None))
        && segment_vh_lean_native(ctx, args, true, 0).is_some()
    {
        return Ok(None);
    }
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    vh_check_arity_row(ctx, args, 1, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    // `asType`'s unbox/widen of the value for a primitive variable -- see
    // `vh_adapt_value_args`. Before this, `set(h, (Object) Integer.valueOf(i))`
    // on an `int` field stored the Integer's ADDRESS.
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    // Re-read: boxing a primitive into a reference variable allocates, and
    // the adapted vector carries the handle's post-GC address.
    let this = obj_arg(args, 0)?;
    // An instance or static field row cannot be an FFM handle (wave 47).
    if !vh_row_is_field(&row) {
        // `SegmentVarHandle` (JEP 454 FFM API): see the matching check in `varhandle_get`.
        if is_segment_var_handle(ctx, this) {
            if let Some(refusal) = segment_vh_ordered_mode_refusal(ctx, this, mode) {
                return Err(refusal);
            }
            if let Some(result) = segment_vh_set(ctx, this, args) {
                if result.is_err() {
                    if let Some(jdk) = segment_vh_raise_through_jdk(ctx, this, args, mode) {
                        return jdk;
                    }
                }
                return result;
            }
        }
        // FFM layout handle — see the matching check in `varhandle_get` for what
        // fell through here before (a silently dropped write).
        if let Some(shape) = p67_segment_vh_shape(ctx, this) {
            return layout_vh_set(ctx, shape, args);
        }
    }
    // Round-7 HIGH-2 fix: bind the Arc once and reuse for kind / field_index
    // / class+field lookups instead of re-locking `vh_meta_table` each branch.
    let meta = vh_row_or_read(ctx, this, &row);
    // byte-array-view (`asLittleEndian.set([BIJ)V` etc.): handle BEFORE the
    // array-element fast path — a view writes `width` bytes at a BYTE index,
    // not a single element. (Writing the wide value into one byte slot was the
    // SHA3/SHAKE-zeros bug.)
    if let Some((elem, le)) = byte_view_kind(meta.as_deref()) {
        if let Some(Value::Object(Some(arr))) = args.get(1).copied() {
            let index = match args.get(2).copied() {
                Some(Value::Int(i)) => i,
                _ => 0,
            };
            let idx = match byte_view_check_index(ctx, arr, index, elem) {
                Ok(i) => i,
                Err(fallback) => {
                    let at = Value::Object(Some(arr));
                    let view = Some((elem, le, false));
                    return Err(vh_index_raise_through_jdk(
                        ctx, this, view, mode, at, index, fallback,
                    ));
                }
            };
            let value = args.get(3).cloned().unwrap_or(Value::Int(0));
            byte_view_set(ctx, arr, idx, elem, le, &value);
        }
        return Ok(None);
    }
    if let Some((elem, le)) = byte_buffer_view_kind(meta.as_deref()) {
        if let Some(Value::Object(Some(mut bb))) = args.get(1).copied() {
            let index = match args.get(2).copied() {
                Some(Value::Int(i)) => i,
                _ => 0,
            };
            if byte_buffer_view_is_read_only(ctx, bb) {
                return Err(RuntimeError::ReadOnlyBufferException.into());
            }
            let idx = match byte_buffer_view_check_index(ctx, bb, index, elem) {
                Ok(i) => i,
                Err(fallback) => {
                    let at = Value::Object(Some(bb));
                    let view = Some((elem, le, true));
                    return Err(vh_index_raise_through_jdk(
                        ctx, this, view, mode, at, index, fallback,
                    ));
                }
            };
            let _window = byte_buffer_view_access_window(ctx, &mut bb)?;
            let value = args.get(3).cloned().unwrap_or(Value::Int(0));
            byte_buffer_view_set(ctx, bb, idx, elem, le, &value);
        }
        return Ok(None);
    }
    // C38: Array-element VarHandle.set — args = [vh, array, idx, value]. Handles
    // real-JDK VarHandleLongs$Array / VarHandleInts$Array / etc.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let value = args.get(3).cloned().unwrap_or(Value::Int(0));
        // Round 15 wave 5 (lane vh7; F3): the value's cast before the index,
        // as HotSpot 25 (`set` only; the RMW arms below check the index first).
        if let Some(refusal) = vh_array_casts_before_index(ctx, this, arr, value, mode) {
            return Err(refusal);
        }
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        if let Some(refusal) = vh_array_refusal_after_index(ctx, this, arr, value, mode) {
            return Err(refusal);
        }
        ctx.set_array_element(arr, idx, value);
        return Ok(None);
    }
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(None),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_INSTANCE => {
            // args[1] = receiver, args[2] = value (signature-polymorphic dispatch)
            let receiver = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(None),
            };
            let value = args.get(2).cloned().unwrap_or(Value::Int(0));
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, Some(value))
            {
                return Err(refusal);
            }

            if field_idx >= 0 {
                ctx.set_field(receiver, field_idx as usize, value);
            } else {
                // By name in the field's holder, not the coordinate.
                if let Some(idx) = vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    ctx.set_field(this, VH_FIELD_INDEX, Value::Int(idx as i32));
                    vh_meta_update_field_index(ctx, this, idx as i32);
                    ctx.set_field(receiver, idx, value);
                }
            }
            Ok(None)
        }
        VH_KIND_STATIC => {
            // args[1] = value
            let value = args.get(1).cloned().unwrap_or(Value::Int(0));
            // Judged like an instance store; see `vh_value_refusal`.
            if let Some(m) = meta.as_deref() {
                if let Some(refusal) = vh_value_refusal(ctx, m, value) {
                    return Err(refusal);
                }
            }
            let mut live = [value];
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            if let Some((cid, sidx)) = vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut live,
                vh_static_init(mode, meta.as_deref()),
            )? {
                ctx.set_static_field(cid, sidx, live[0]);
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// `VarHandle.setVolatile` — [`varhandle_set`] between two SeqCst fences.
///
/// `varhandle_set` stores with the PLAIN store (`set_field`, `set_array_element`,
/// `set_static_field`, the view/segment writers), which is right for `set`,
/// `setOpaque` and `setRelease` on x86-64 but not for `setVolatile`: a volatile
/// store must not be reordered with a LATER volatile load (StoreLoad), which
/// is what a Dekker handshake written with `setVolatile`/`getVolatile` relies
/// on. The trailing fence is that barrier (HotSpot's `lock addl`); the leading
/// one orders every earlier access before the store, which `setRelease`
/// already gets from x86-64 TSO but a weaker target would not. Wrapping the
/// whole native rather than its instance arm covers every handle shape the
/// native serves — static, array element, byte view, segment — with one rule.
pub fn varhandle_set_volatile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    let result = varhandle_set_mode(ctx, args, "setVolatile");
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    result
}

/// VarHandle.compareAndSet(receiver, expected, new) → boolean
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver, expected, new_value] for instance fields.
pub fn varhandle_compare_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_compare_and_set_mode(ctx, args, "compareAndSet")
}

/// [`varhandle_compare_and_set`] for `mode` (`compareAndSet` and the four
/// `weakCompareAndSet*`).
pub fn varhandle_compare_and_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 2, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 2, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 2, mode)?;
    let kind = vh_check_arity_row(ctx, args, 2, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    // BOTH values are adapted: an `Integer` expectation through an `Object`
    // site never matched an `int` slot, so the CAS always failed.
    let adapted = vh_adapt_value_args_row(ctx, this, args, 2, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::CompareAndSet, mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`. Not for an
    // instance or static field row (wave 47).
    if !vh_row_is_field(&row) {
        if let Some(shape) = p67_segment_vh_shape(ctx, obj_arg(args, 0)?) {
            return layout_vh_rmw(ctx, shape, args, VhViewRmw::CompareAndSet, mode);
        }
    }
    if is_byte_buffer_view_kind(kind) {
        return byte_buffer_view_rmw(ctx, args, VhViewRmw::CompareAndSet);
    }
    // Re-read, as in `varhandle_set`: the adapted vector is post-allocation.
    let this = obj_arg(args, 0)?;
    // C38: Array-element CAS — args = [vh, array, idx, expected, new_value].
    // Index first: HotSpot 25 answers an out-of-bounds CAS with a wrong-typed
    // value AIOOBE (`R15Vh7ArrayCastOrder` row `casOob`); only `set` casts
    // first (round 15 wave 5, lane vh7).
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let expected = args.get(3).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(4).cloned().unwrap_or(Value::Int(0));
        // Round 15 wave 4 (lane vh5): the expectation is cast first.
        if let Some(refusal) = vh_array_expected_refusal(ctx, this, arr, expected) {
            return Err(refusal);
        }
        if let Some(refusal) = vh_array_store_refusal(ctx, this, arr, new_val) {
            return Err(refusal);
        }
        // LOW-finding fix: atomic compare-and-set. The old get/compare/set was
        // a non-atomic RMW (could spuriously succeed against a value another
        // thread changed). `compare_and_swap_field` does the compare+write
        // under the VM's per-element CAS lock and returns whether it swapped.
        let ok = ctx.compare_and_swap_field(arr, idx, expected, new_val);
        return Ok(Some(Value::Int(ok as i32)));
    }
    // Round-7 HIGH-2 fix: fetch meta once and reuse across the kind probe
    // and the class/field fallback below.
    let meta = vh_row_or_read(ctx, this, &row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Int(0))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    if kind == VH_KIND_STATIC {
        // Static CAS — args = [vh, expected, new_value]. (Array CAS was already
        // routed above via `vh_array_call`.)
        let expected = args.get(1).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(2).cloned().unwrap_or(Value::Int(0));
        // The expectation, then the NEW value, are cast to the variable's
        // type, as the JDK's body casts them (nio21 wave 12, lane L3; see
        // `vh_static_cas_refusal`).
        if let Some(m) = meta.as_deref() {
            if let Some(refusal) = vh_static_cas_refusal(ctx, m, expected, new_val) {
                return Err(refusal);
            }
        }
        let mut live = [expected, new_val];
        // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
        let Some((cid, sidx)) = vh_static_slot_of(
            ctx,
            this,
            meta.as_deref(),
            &mut live,
            vh_static_init(mode, meta.as_deref()),
        )?
        else {
            return Ok(Some(Value::Int(0)));
        };
        let [expected, new_val] = live;
        // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`, default ON):
        // one hardware CAS on the static cell first, atomic against compiled
        // `putstatic`; the mirror-monitor lock below is the fallback.
        if let Some((_, ok)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
            vh_values_match(&current, &expected).then_some(new_val)
        }) {
            return Ok(Some(Value::Int(ok as i32)));
        }
        // LOW-finding fix: atomic static CAS — serialize the compare+set on the
        // per-class mirror monitor (no CAS primitive exists for static slots).
        let ok = vh_with_static_lock(ctx, cid, &[expected, new_val], |ctx, live| {
            let (expected, new_val) = (live[0], live[1]);
            // JIT round 22 wave 5c, lane sync: the hardware arm again under the
            // lock (a cell typed since the first try), so the lock path and a
            // hardware RMW of the slot cannot both succeed.
            if let Some((_, ok)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
                vh_values_match(&current, &expected).then_some(new_val)
            }) {
                return ok;
            }
            let current = ctx.get_static_field(cid, sidx);
            if vh_values_match(&current, &expected) {
                ctx.set_static_field(cid, sidx, new_val);
                true
            } else {
                false
            }
        });
        return Ok(Some(Value::Int(ok as i32)));
    }
    if kind != VH_KIND_INSTANCE {
        // Unsupported VarHandle kind — report CAS failure rather than a
        // false success.
        return Ok(Some(Value::Int(0)));
    }

    // args[1] = receiver, args[2] = expected, args[3] = new_value
    let receiver = match args.get(1).copied() {
        Some(Value::Object(Some(r))) => r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected = args.get(2).cloned().unwrap_or(Value::Int(0));
    let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));
    // The receiver, the expectation and the NEW value are cast in the JDK's
    // argument order. A wrong-typed expectation used to fail the comparison
    // here (answering `false`); HotSpot 25's body casts it
    // (`fieldType.cast(expected)`) and throws `ClassCastException` (nio21
    // wave 12, lane L3; `vh_instance_cas_refusal`).
    if let Some(refusal) =
        vh_instance_cas_refusal(ctx, meta.as_deref(), receiver, expected, new_val)
    {
        return Err(refusal);
    }

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        // By name in the field's holder, not the coordinate.
        match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                vh_meta_update_field_index(ctx, this, i as i32);
                i
            }
            None => return Ok(Some(Value::Int(0))),
        }
    };

    // LOW-finding fix: atomic compare-and-set on the instance field. The old
    // get/compare/set was a non-atomic RMW (could spuriously succeed against a
    // value another thread changed between the read and the write).
    // `compare_and_swap_field` does the compare+write under the VM's per-object
    // CAS lock and returns whether the swap happened.
    let ok = ctx.compare_and_swap_field(receiver, idx, expected, new_val);
    Ok(Some(Value::Int(ok as i32)))
}

/// VarHandle.compareAndExchange(receiver, expected, new) → witness value
/// Signature-polymorphic: args = [vh_ref, receiver, expected, new_value]
///
/// Boxes through [`vh_box_access_result`] — see there for why the erased
/// `Object` call site needs it and why the boxing cannot live at the
/// poly-return boundary.
pub fn varhandle_compare_and_exchange(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_compare_and_exchange_mode(ctx, args, "compareAndExchange")
}

/// [`varhandle_compare_and_exchange`] for `mode` (and its `Acquire` /
/// `Release` variants).
pub fn varhandle_compare_and_exchange_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    let own = vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 2, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 2, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 2, mode)?;
    let kind = vh_check_arity_row(ctx, args, 3, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 2, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) =
        segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::CompareAndExchange, mode)
    {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::CompareAndExchange, mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::CompareAndExchange)
    } else {
        varhandle_compare_and_exchange_raw(ctx, args, &row, mode)
    };
    let result = vh_box_access_result(ctx, args, raw);
    vh_own_wrapper_null(ctx, own, result)
}

pub fn varhandle_compare_and_exchange_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element compareAndExchange — args = [vh, array, idx, expected, new].
    // Index first, as HotSpot 25 (`R15Vh7ArrayCastOrder` row `caeOob`).
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let expected = args.get(3).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(4).cloned().unwrap_or(Value::Int(0));
        // Round 15 wave 4 (lane vh5): the expectation is cast first.
        if let Some(refusal) = vh_array_expected_refusal(ctx, this, arr, expected) {
            return Err(refusal);
        }
        if let Some(refusal) = vh_array_store_refusal(ctx, this, arr, new_val) {
            return Err(refusal);
        }
        // LOW-finding fix: atomic compare-and-exchange. The previous
        // get/compare/set was a non-atomic RMW (lost updates under
        // contention). `compare_and_swap_field` runs the compare+write under
        // the VM's per-element CAS lock (its impl handles array refs).
        // compareAndExchange returns the *witness*; see
        // `vh_compare_and_exchange_slot` (interpreter round i2 wave 55, lane
        // L7) for why a failed CAS is not answered by one re-read.
        return Ok(Some(vh_compare_and_exchange_slot(
            ctx, arr, idx, true, expected, new_val,
        )));
    }
    // Meta side-table FIRST — real-JDK VarHandles don't carry our synthetic
    // 6-field layout (see `varhandle_get_and_bitwise`).
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    if kind == VH_KIND_STATIC {
        // Static compareAndExchange — args = [vh, expected, new_value]; returns
        // the witness (value found), updated iff it equalled `expected`.
        let expected = args.get(1).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(2).cloned().unwrap_or(Value::Int(0));
        // The expectation, then the NEW value, are cast to the variable's
        // type, as the JDK's body casts them (nio21 wave 12, lane L3; see
        // `vh_static_cas_refusal`).
        if let Some(m) = meta.as_deref() {
            if let Some(refusal) = vh_static_cas_refusal(ctx, m, expected, new_val) {
                return Err(refusal);
            }
        }
        let mut live = [expected, new_val];
        // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
        let Some((cid, sidx)) = vh_static_slot_of(
            ctx,
            this,
            meta.as_deref(),
            &mut live,
            vh_static_init(mode, meta.as_deref()),
        )?
        else {
            return Ok(Some(Value::Object(None)));
        };
        let [expected, new_val] = live;
        // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`, default ON):
        // one hardware CAS on the static cell first, atomic against compiled
        // `putstatic`; the mirror-monitor lock below is the fallback.
        if let Some((current, _)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
            vh_values_match(&current, &expected).then_some(new_val)
        }) {
            return Ok(Some(current));        }
        // LOW-finding fix: no CAS primitive exists for static slots, so make
        // the compare-and-exchange atomic by serializing get+set on the
        // per-class mirror monitor (see `vh_with_static_lock`).
        let current = vh_with_static_lock(ctx, cid, &[expected, new_val], |ctx, live| {
            let (expected, new_val) = (live[0], live[1]);
            // JIT round 22 wave 5c, lane sync: the hardware arm again under the
            // lock (a cell typed since the first try), so the lock path and a
            // hardware RMW of the slot cannot both succeed.
            if let Some((current, _)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
                vh_values_match(&current, &expected).then_some(new_val)
            }) {
                return current;
            }
            let current = ctx.get_static_field(cid, sidx);
            if vh_values_match(&current, &expected) {
                ctx.set_static_field(cid, sidx, new_val);
            }
            current
        });
        return Ok(Some(current));
    }
    if kind != VH_KIND_INSTANCE {
        return Ok(Some(Value::Object(None)));
    }

    let receiver = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let expected = args.get(2).cloned().unwrap_or(Value::Int(0));
    let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));
    // MEASURED on HotSpot 25.0.3+9 before wiring, `probes/ReflectArgTypeSweep.java`:
    // `getAndSet`, `getAndAdd` and `compareAndExchange` all raise
    // ClassCastException on a wrong receiver too, so all three doors get the
    // same check as `set`/`get`/`compareAndSet`. They were left out of the
    // first pass because only the latter three had been measured, and an
    // unmeasured door is where a fix at the wrong level starts. The
    // expectation is cast too, before the new value (nio21 wave 12, lane L3;
    // `vh_instance_cas_refusal`).
    if let Some(refusal) =
        vh_instance_cas_refusal(ctx, meta.as_deref(), receiver, expected, new_val)
    {
        return Err(refusal);
    }

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        // By name in the field's holder, not the coordinate.
        match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                i
            }
            None => return Ok(Some(Value::Object(None))),
        }
    };

    // LOW-finding fix: atomic compare-and-exchange on the instance field.
    // The old get/compare/set was a non-atomic RMW and lost updates under
    // contention; `compare_and_swap_field` performs the compare+write under
    // the VM's per-object CAS lock. compareAndExchange returns the witness:
    // `expected` on a successful swap, else a value the field held that is
    // not `expected` (`vh_compare_and_exchange_slot`).
    Ok(Some(vh_compare_and_exchange_slot(
        ctx, receiver, idx, false, expected, new_val,
    )))
}

/// The witness of a `VarHandle.compareAndExchange` on slot `idx` of `obj` (an
/// array element when `is_array`): `expected` when the exchange happened,
/// otherwise a value the variable held at some instant during the call that
/// is NOT `expected` -- HotSpot's one `cmpxchg` answers with exactly such a
/// value (interpreter round i2 wave 55, lane L7;
/// `docs/feature-designs/profile-guided-inlining.md`).
///
/// The trait's CAS answers only "swapped or not". The witness used to be one
/// re-read after a failed CAS, and another thread may store `expected` in
/// between: the caller then saw `expected` although nothing was exchanged (a
/// `while ((w = cae(e, n)) != e) e = w;` loop exits without writing `n`, a
/// lost update). Here a re-read that equals `expected` is not an answer: the
/// exchange is attempted again, as the `Unsafe.compareAndExchange*` natives'
/// read-then-CAS loop does (`unsafe_compare_exchange_heap_slot`). The common
/// success path is still one CAS. `vh_values_match` is no looser than the
/// VM's CAS equality (`vm_exec::values_equal_for_cas`), so a re-read it calls
/// equal is one the next CAS compares equal too unless the variable moved
/// again. A context whose CAS equality is NARROWER (a test double comparing
/// floats with `==`, so a NaN never compares equal) would never converge, so
/// the retries of a FLOATING expectation are bounded (4096) and the last
/// re-read answers. An `int` / `long` / reference expectation gets a bound
/// 256 times larger (interpreter round i4 wave 92, lane L4): the VM's CAS
/// equality on those is exactly `vh_values_match`'s, so the loop ends unless
/// another thread restores `expected` between EVERY CAS and its re-read, and
/// the old 4096 could then answer `expected` for an exchange that did not
/// happen -- the witness HotSpot's `cmpxchg` never gives (probe
/// `L4W92CaeWitnessContention`). Not unbounded: a test double whose CAS reads
/// a slot differently from the re-read (an array element through
/// `get_field`) must still terminate.
pub fn vh_compare_and_exchange_slot(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    idx: usize,
    is_array: bool,
    expected: Value,
    new_val: Value,
) -> Value {
    let max_equal_rereads: u32 = if matches!(expected, Value::Float(_) | Value::Double(_)) {
        1 << 12
    } else {
        1 << 20
    };
    let mut attempts = 0u32;
    loop {
        if ctx.compare_and_swap_field(obj, idx, expected.clone(), new_val.clone()) {
            return expected;
        }
        let current = if is_array {
            ctx.get_array_element(obj, idx)
        } else {
            ctx.get_field_volatile(obj, idx)
        };
        attempts += 1;
        if !vh_values_match(&current, &expected) || attempts >= max_equal_rereads {
            return if is_array {
                current
            } else {
                crate::unsafe_natives::slot0_read_keep_alive(ctx, obj, idx, current)
            };
        }
        if attempts % 64 == 0 {
            std::thread::yield_now();
        }
    }
}

/// VarHandle.getAndSet(receiver, new) → old value
/// Signature-polymorphic: args = [vh_ref, receiver, new_value]
///
/// Boxes through [`vh_box_access_result`] — see there for why the erased
/// `Object` call site needs it and why the boxing cannot live at the
/// poly-return boundary.
pub fn varhandle_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_set_mode(ctx, args, "getAndSet")
}

/// [`varhandle_get_and_set`] for `mode` (and its `Acquire` / `Release`
/// variants).
pub fn varhandle_get_and_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    let own = vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    let kind = vh_check_arity_row(ctx, args, 4, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::GetAndSet, mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::GetAndSet, mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::GetAndSet)
    } else {
        varhandle_get_and_set_raw(ctx, args, &row, mode)
    };
    let result = vh_box_access_result(ctx, args, raw);
    vh_own_wrapper_null(ctx, own, result)
}

/// Where a [`vh_cas_rmw`] slot lives: an array element or an instance field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VhRmwSlot {
    ArrayElement,
    Field,
}

/// Yield the CPU every this many failed CASes of one [`vh_cas_rmw`].
pub const VH_RMW_YIELD_EVERY: usize = 1024;

/// The atomic read-modify-write behind the native `VarHandle` `getAndSet`,
/// `getAndAdd` and `getAndBitwise*` on an array element or an instance field:
/// read, compute `update(old)`, CAS, and retry until the CAS lands; answers
/// the `old` it replaced.
///
/// nio21 wave 6 L1
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// these loops gave up after 1024 failed CASes and finished with a plain read
/// and a plain store, so an update landing between the two was lost and two
/// threads in the fallback could both answer the same `old`. HotSpot never
/// loses one. A failed CAS means another thread's update landed (the element
/// CAS is a hardware atomic since nio21 wave 5 L3), so the loop terminates;
/// it yields every [`VH_RMW_YIELD_EVERY`] failures, as the `Unsafe` CAS loops
/// do. `CRATONVM_VH_RMW_UNBOUNDED=0` restores the bounded loop with the plain
/// fallback (read only once 1024 CASes have failed).
pub fn vh_cas_rmw(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    idx: usize,
    slot: VhRmwSlot,
    update: impl Fn(&Value) -> Value,
) -> Value {
    fn read(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize, slot: VhRmwSlot) -> Value {
        match slot {
            VhRmwSlot::ArrayElement => ctx.get_array_element(obj, idx),
            VhRmwSlot::Field => ctx.get_field_volatile(obj, idx),
        }
    }
    // The pre-wave-6 fallback: a plain read and a plain store.
    fn plain_store(
        ctx: &mut dyn NativeContext,
        obj: ObjectRef,
        idx: usize,
        slot: VhRmwSlot,
        update: &dyn Fn(&Value) -> Value,
    ) -> Value {
        let old = read(&*ctx, obj, idx, slot);
        let new_val = update(&old);
        match slot {
            VhRmwSlot::ArrayElement => ctx.set_array_element(obj, idx, new_val),
            VhRmwSlot::Field => ctx.set_field_volatile(obj, idx, new_val),
        }
        old
    }
    let mut failures = 0usize;
    // nio21 wave 7 L1: consecutive failed CASes whose read answered the
    // bit-identical value. Under contention a failure means the slot changed
    // between our read and our CAS, so the next read differs (it can only
    // repeat after an A-B-A between every read and CAS); a long run of
    // identical reads is a CAS that can never match what the read decodes
    // (an `Uninitialized` slot, which `values_equal_for_cas` never equates,
    // or a context whose compare is not bitwise, such as a `NaN` under a
    // `PartialEq` compare). The unbounded loop would spin there forever.
    let mut same_reads = 0usize;
    let mut last_read: Option<Value> = None;
    loop {
        let old = read(&*ctx, obj, idx, slot);
        let new_val = update(&old);
        if ctx.compare_and_swap_field(obj, idx, old, new_val) {
            return old;
        }
        failures += 1;
        if last_read.is_some_and(|l| vh_rmw_same_bits(&l, &old)) {
            same_reads += 1;
        } else {
            same_reads = 0;
        }
        last_read = Some(old);
        if same_reads == VH_RMW_STUCK_AFTER
        {
            // Not contention: the CAS cannot land. Finish as a single-thread
            // HotSpot would (`update(old)` stored, `old` answered) rather than
            // hang the thread. `CRATONVM_VH_RMW_STUCK_GUARD=0` keeps spinning.
            return plain_store(ctx, obj, idx, slot, &update);
        }
        if failures % VH_RMW_YIELD_EVERY == 0 {
            if failures == VH_RMW_YIELD_EVERY
                && false
            {
                return plain_store(ctx, obj, idx, slot, &update);
            }
            std::thread::yield_now();
        }
    }
}

/// Consecutive bit-identical failed reads after which [`vh_cas_rmw`] decides
/// its CAS cannot land (see `same_reads` there).
pub const VH_RMW_STUCK_AFTER: usize = 1 << 16;

/// Bit equality of two read values: `to_bits` for `float` / `double` (a `NaN`
/// equals itself), `PartialEq` for everything else.
pub fn vh_rmw_same_bits(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}


pub fn varhandle_get_and_set_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element getAndSet — args = [vh, array, idx, new_value].
    // Index first, as HotSpot 25 (`R15Vh7ArrayCastOrder` row `gasOob`).
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));
        if let Some(refusal) = vh_array_store_refusal(ctx, this, arr, new_val) {
            return Err(refusal);
        }
        // Atomic getAndSet: a CAS loop until it lands (`vh_cas_rmw`;
        // `compare_and_swap_field` runs on array refs). nio21 wave 6 L1: the
        // plain-store fallback after 1024 failures is gone.
        let old = vh_cas_rmw(ctx, arr, idx, VhRmwSlot::ArrayElement, |_| new_val);
        return Ok(Some(old));
    }
    // Meta side-table FIRST — real-JDK VarHandles don't carry our synthetic
    // 6-field layout (see `varhandle_get_and_bitwise`).
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    if kind == VH_KIND_STATIC {
        // Static getAndSet — args = [vh, new_value]; returns the old value.
        let new_val = args.get(1).cloned().unwrap_or(Value::Int(0));
        // Judged like an instance store; see `vh_value_refusal`.
        if let Some(m) = meta.as_deref() {
            if let Some(refusal) = vh_value_refusal(ctx, m, new_val) {
                return Err(refusal);
            }
        }
        let mut live = [new_val];
        // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
        let Some((cid, sidx)) = vh_static_slot_of(
            ctx,
            this,
            meta.as_deref(),
            &mut live,
            vh_static_init(mode, meta.as_deref()),
        )?
        else {
            return Ok(Some(Value::Object(None)));
        };
        let [new_val] = live;
        // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`, default ON):
        // one hardware CAS on the static cell first, atomic against compiled
        // `putstatic`; the mirror-monitor lock below is the fallback.
        if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |_| Some(new_val)) {
            return Ok(Some(old));
        }
        // LOW-finding fix: atomic static getAndSet — serialize get+set on the
        // per-class mirror monitor (no CAS primitive for static slots).
        let old = vh_with_static_lock(ctx, cid, &[new_val], |ctx, live| {
            let new_val = live[0];
            // JIT round 22 wave 5c, lane sync: the hardware arm again under the
            // lock (a cell typed since the first try), so the lock path and a
            // hardware RMW of the slot cannot both succeed.
            if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |_| Some(new_val)) {
                return old;
            }
            let old = ctx.get_static_field(cid, sidx);
            ctx.set_static_field(cid, sidx, live[0]);
            old
        });
        return Ok(Some(old));
    }
    if kind != VH_KIND_INSTANCE {
        return Ok(Some(Value::Object(None)));
    }

    let receiver = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(2).cloned().unwrap_or(Value::Int(0));
    // MEASURED on HotSpot 25.0.3+9 before wiring, `probes/ReflectArgTypeSweep.java`:
    // `getAndSet`, `getAndAdd` and `compareAndExchange` all raise
    // ClassCastException on a wrong receiver too, so all three doors get the
    // same check as `set`/`get`/`compareAndSet`. They were left out of the
    // first pass because only the latter three had been measured, and an
    // unmeasured door is where a fix at the wrong level starts.
    if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, Some(new_val)) {
        return Err(refusal);
    }

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        // By name in the field's holder, not the coordinate.
        match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                i
            }
            None => return Ok(Some(Value::Object(None))),
        }
    };

    // Atomic instance getAndSet: a CAS loop until it lands (`vh_cas_rmw`;
    // nio21 wave 6 L1 dropped the plain-store fallback).
    let old = vh_cas_rmw(ctx, receiver, idx, VhRmwSlot::Field, |_| new_val);
    Ok(Some(old))
}

/// VarHandle.getAndAdd(receiver, delta) → old value.
///
/// Signature-polymorphic for both array and instance-field VarHandles.
/// - Array kind: args = [vh, array, index, delta] (H2's `VarHandleInts$Array.getAndAdd([III)I`).
/// - Instance kind: args = [vh, receiver, delta].
/// - Static kind: args = [vh, delta].
///
/// Delta can be Int, Long, Float, or Double. Returns the previous value BOXED
/// through [`vh_box_access_result`], which the erased `Object` call site
/// requires; `unbox_poly_return` then unwraps it for a primitive call site such
/// as H2's `([III)I`.
/// `CRATONVM_VH_READ_ONLY_HANDLE_UOE=0` — restore the pre-2026-09-02 behaviour,
/// in which a `VarHandle` over a FINAL field performed the write.
///
/// Default on, and a third switch beside `CRATONVM_VH_NULL_COORDINATE_NPE` and
/// `CRATONVM_VH_UNSUPPORTED_MODE_UOE` rather than a shared one, for the reason
/// those two are separate: the rules interact, and a shared switch could not
/// isolate any of them. This is the one of the three that can change a
/// PASSING workload, because code that writes a final field succeeds today.
pub fn vh_read_only_handle_uoe_enabled() -> bool {
    !matches!(
        None::<String>.as_deref(),
        Some("0")
    )
}

/// JVMS §4.5 `ACC_FINAL`.
pub const ACC_FINAL: u16 = 0x0010;

/// Is `field_name` declared `final` on `class_id` or an ancestor?
///
/// Walks the superclass chain exactly as `lookup_require_field` does, and for
/// the same reason: the field a handle names may be inherited. Called ONCE per
/// handle, at `findVarHandle` time, never on an access.
pub fn vh_field_is_final(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    field_name: &str,
) -> bool {
    let mut cid = class_id;
    loop {
        for f in ctx.declared_fields(cid) {
            if f.name == field_name {
                return f.access_flags & ACC_FINAL != 0;
            }
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => return false,
        }
    }
}

/// Mark an already-built handle's metadata read-only.
///
/// Separate from `alloc_instance_var_handle` rather than a parameter on it:
/// that helper has several callers which mint handles for variables that are
/// never final (array elements, byte views), and a parameter would put the
/// question to all of them. The two `Lookup.find*VarHandle` natives are the
/// only places a final field can enter.
///
/// Answers `vh`'s current address: under `CRATONVM_VH_REAL_VARFORM` the handle
/// is given its read-only `VarForm` here ([`vh_install_real_varform`]), and
/// building one is a GC point (interpreter round i6 wave 123, lane L4).
pub fn vh_mark_read_only(ctx: &mut dyn NativeContext, vh: ObjectRef) -> ObjectRef {
    if let Some(meta) = vh_meta_get(ctx, vh) {
        let mut updated = (*meta).clone();
        updated.read_only = true;
        vh_meta_put(ctx, vh, updated);
        return vh_install_real_varform(ctx, vh);
    }
    vh
}

/// `UnsupportedOperationException` for a WRITE mode on a read-only handle.
///
/// # The rule
///
/// `findVarHandle` on a `final` field yields a handle whose write modes are all
/// unsupported. Measured on JDK 25 with `RJdkVarHandleModeSupport`: every one
/// of `set` / `setVolatile` / `setOpaque` / `setRelease`, the five CAS modes,
/// the three compare-and-exchange modes, `getAndSet*`, `getAndAdd*` and the
/// nine `getAndBitwise*` raise it; the four READ modes still answer, and so
/// does a read of a final REFERENCE field and of a `static final`.
///
/// CratonVM performed the write. `VH.set(h, 5)` stored 5 into a `final int` —
/// a silent successful write to a field the language guarantees is immutable,
/// which anything caching that field's value is entitled to assume cannot
/// happen.
///
/// # Where it sits
///
/// FIRST, ahead of both other rules. Measured, not chosen:
/// `VH_FIN.set((H) null, 5)` on a final-field handle raises
/// `UnsupportedOperationException`, not `NullPointerException`, so read-only
/// beats the null coordinate. Read-only against an unsupported MODE is not
/// observable — both raise `UnsupportedOperationException` — so the order
/// between those two is free and this one is placed first for both.
pub fn vh_check_read_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> Result<(), MethodCallFailed> {
    vh_check_read_only_row(ctx, args, mode, &None)
}

/// [`vh_check_read_only`] over the row the prologue read (wave 47,
/// [`VARHANDLE_ONE_ROW_READ`]).
pub fn vh_check_read_only_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
    row: &VhPrologueRow,
) -> Result<(), MethodCallFailed> {
    let Some(this) = args.first().and_then(|v| match v {
        Value::Object(Some(o)) => Some(*o),
        _ => None,
    }) else {
        return Ok(());
    };
    let read_only = vh_row_or_read(ctx, this, row)
        .as_deref()
        .map(|m| m.read_only)
        .unwrap_or(false);
    // The kill switch is asked only for a read-only handle (wave 47): it is
    // a flag-table lookup by name, which every `set` and `compareAndSet`
    // paid before the row was even read.
    if !read_only || !vh_read_only_handle_uoe_enabled() {
        return Ok(());
    }
    // HotSpot's message is the access mode's method name (`set`), as for
    // every unsupported mode (interpreter round i1 wave 32).
    Err(RuntimeError::UnsupportedOperationException {
        message: mode.to_string(),
    }
    .into())
}

/// `CRATONVM_VH_UNSUPPORTED_MODE_UOE=0` — restore the pre-2026-09-02 behaviour,
/// in which an access mode the variable's type does not admit ANSWERED instead
/// of raising `UnsupportedOperationException`.
///
/// Default on, and separate from `CRATONVM_VH_NULL_COORDINATE_NPE` on purpose:
/// the two rules interact (this one wins, which is HotSpot's order), so a
/// single switch for both could not isolate either. Each turns silence into an
/// exception on a path any workload can reach, and a suite that starts failing
/// has to be bisectable to the RULE rather than to a rebuild.
pub fn vh_unsupported_mode_uoe_enabled() -> bool {
    !matches!(
        None::<String>.as_deref(),
        Some("0")
    )
}

/// Which family of access mode is being attempted, for
/// [`vh_check_access_mode_supported`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VhModeFamily {
    /// `getAndAdd`, `getAndAddAcquire`, `getAndAddRelease`.
    Arithmetic,
    /// `getAndBitwise{Or,And,Xor}` and their ordering variants.
    Bitwise,
}

/// `UnsupportedOperationException` for an access mode the VARIABLE'S TYPE does
/// not admit.
///
/// # The rule, measured rather than assumed
///
/// It is not uniform, which is why `RJdkVarHandleModeSupport` sweeps every mode
/// against every variable type instead of picking a few. On JDK 25:
///
/// ```text
///   variable      getAndAdd*   getAndBitwise*
///   boolean       UNSUPPORTED  ok
///   byte char short int long   ok           ok
///   float double  ok           UNSUPPORTED
///   any reference UNSUPPORTED  UNSUPPORTED
/// ```
///
/// So arithmetic is the NUMERIC primitives and bitwise is the INTEGRAL ones
/// plus `boolean` — the two sets differ at both ends, and `boolean` versus
/// `float` is the pair that catches a guess. The `Acquire`/`Release` variants
/// follow their base mode exactly (checked for all four of `boolean`, `int`,
/// `float` and a reference).
///
/// # Why it runs BEFORE the null-coordinate check
///
/// Because HotSpot's does. Swept across all ten types: where the mode is
/// unsupported, a null receiver still yields `UnsupportedOperationException`;
/// where it is supported, the null receiver yields `NullPointerException`. The
/// two rules never disagree about which wins, so the order here is the whole
/// of the interaction and `vh_check_leading_coordinate` is called after this.
///
/// # What CratonVM did before
///
/// Answered. 57 of the sweep's 166 rows raise `UnsupportedOperationException`
/// on HotSpot, and CratonVM returned a value for every one — `null` 28 times,
/// a `float` 20 times (a bitwise op on a `float` variable), `false`/`true` 5
/// times, and two silent write successes.
///
/// # Cost
///
/// One `Arc` deref and one byte compare on the ordinary path: the meta is
/// already in the per-thread memo every access mode consults, and the
/// descriptor's FIRST byte is the whole test. No allocation, and no work at
/// all for the read/write/CAS families, which do not call this.
pub fn vh_check_access_mode_supported(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    family: VhModeFamily,
    mode: &'static str,
) -> Result<(), MethodCallFailed> {
    if !vh_unsupported_mode_uoe_enabled() {
        return Ok(());
    }
    let Some(this) = args.first().and_then(|v| match v {
        Value::Object(Some(o)) => Some(*o),
        _ => None,
    }) else {
        return Ok(());
    };
    // A `SegmentVarHandle` is a real JDK class with its own form; its mode
    // support was not swept, so it keeps today's behaviour.
    if is_segment_var_handle(ctx, this) {
        return Ok(());
    }
    // The VARIABLE's descriptor, cheapest source first. An array-element
    // handle minted by the real JDK has no side-table entry, and there the
    // ELEMENT type is what the rule is about — `real_array_var_handle_descriptors`
    // is the same accessor `vh_value_and_coordinate_descriptors` reaches for.
    let meta = vh_meta_get(ctx, this);
    let first = match meta.as_deref() {
        Some(m) => m.field_desc.as_bytes().first().copied(),
        None => match real_array_var_handle_descriptors(ctx, this) {
            Some((value_desc, _)) => value_desc.as_bytes().first().copied(),
            // `vh_read_string`, NOT `vh_field_desc`. The latter substitutes
            // `DESC_OBJECT` when the handle carries no descriptor at all, so an
            // UNDETERMINABLE descriptor arrives here spelled `L...` and the
            // `let Some(first) = first else { return Ok(()) }` escape two lines
            // below can never fire. The comment on that escape says the
            // intended behaviour is to keep today's rather than "inheriting a
            // refusal nothing measured", and a default is not a measurement.
            //
            // MEASURED: `get_and_add_on_long_array_boxes_long` builds a handle
            // with no metadata and a real `long[]` coordinate, and got
            // `UnsupportedOperationException: getAndAdd is not supported for a
            // VarHandle over a variable of type a reference` -- the fallback
            // describing itself.
            None => {
                vh_read_string(ctx, this, VH_FIELD_DESC).and_then(|s| s.as_bytes().first().copied())
            }
        },
    };
    // An undeterminable descriptor keeps today's behaviour rather than
    // inheriting a refusal nothing measured.
    let Some(first) = first else {
        return Ok(());
    };
    let supported = match family {
        VhModeFamily::Arithmetic => {
            matches!(first, b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
        }
        VhModeFamily::Bitwise => {
            matches!(first, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J')
        }
    };
    if supported {
        return Ok(());
    }
    // HotSpot's message is the access mode's method name (`getAndAdd`),
    // as for every unsupported mode (interpreter round i1 wave 32).
    Err(RuntimeError::UnsupportedOperationException {
        message: mode.to_string(),
    }
    .into())
}

pub fn varhandle_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_add_mode(ctx, args, "getAndAdd")
}

/// [`varhandle_get_and_add`] for `mode` (and its `Acquire` / `Release`
/// variants).
pub fn varhandle_get_and_add_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    vh_check_access_mode_supported(ctx, args, VhModeFamily::Arithmetic, mode)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    let kind = vh_check_arity_row(ctx, args, 4, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    // The delta is adapted like any stored value: `getAndAdd(h, 1)` on a
    // `float` field reached `add_values(Float, Int)`, which has no arm for
    // the pair and returned the current value unchanged -- a lost add.
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::GetAndAdd, mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::GetAndAdd, mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::GetAndAdd)
    } else {
        varhandle_get_and_add_raw(ctx, args, &row, mode)
    };
    vh_box_access_result(ctx, args, raw)
}

pub fn varhandle_get_and_add_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    // Interpreter round i5 wave 102, lane L4: the sum is narrowed to the
    // variable's own type, as `Unsafe.getAndAddByte` / `Short` / `Char` store
    // `(byte) (v + delta)`: a `byte` 127 plus 1 is -128, a `char` 65535 plus 1
    // is 0. The plain `int` sum left 128 / 65536 in a `byte` / `char` static or
    // `Value`-cell field (probe `L4W102VarHandleSubwordGetAndAdd`).
    fn add_values_narrowed(current: &Value, delta: &Value, var_desc: &str) -> Value {
        match (add_values(current, delta), var_desc.as_bytes().first()) {
            (Value::Int(v), Some(b'B')) => Value::Int(i32::from(v as i8)),
            (Value::Int(v), Some(b'S')) => Value::Int(i32::from(v as i16)),
            (Value::Int(v), Some(b'C')) => Value::Int(i32::from(v as u16)),
            (sum, _) => sum,
        }
    }

    fn add_values(current: &Value, delta: &Value) -> Value {
        match (current, delta) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
            (Value::Long(a), Value::Long(b)) => Value::Long(a.wrapping_add(*b)),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Double(a), Value::Double(b)) => Value::Double(a + b),
            // Mixed: widen delta to current's type.
            (Value::Long(a), Value::Int(b)) => Value::Long(a.wrapping_add(*b as i64)),
            (Value::Int(a), Value::Long(b)) => Value::Int((*a as i64).wrapping_add(*b) as i32),
            _ => current.clone(),
        }
    }

    // LOW-finding fix: atomic getAndAdd on an array element via a bounded CAS
    // retry loop (was a non-atomic get/add/set → lost updates under
    // contention). The add is recomputed from the freshly-read `old` each
    // iteration so a racing update is never clobbered.
    // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`); the plain-store
    // fallback after 1024 failures lost updates.
    fn array_get_and_add(
        ctx: &mut dyn NativeContext,
        arr: ObjectRef,
        idx: usize,
        delta: &Value,
    ) -> Value {
        let elem_desc = array_element_desc(&*ctx, arr);
        vh_cas_rmw(ctx, arr, idx, VhRmwSlot::ArrayElement, |old| {
            add_values_narrowed(old, delta, elem_desc)
        })
    }

    // C38: Array-element getAndAdd — args = [vh, array, idx, delta]. Route first
    // so real-JDK VarHandleLongs$Array / VarHandleInts$Array calls work even
    // when the VH's slot 0 is not our synthetic VH_KIND Int.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let delta = args.get(3).cloned().unwrap_or(Value::Int(0));
        return Ok(Some(array_get_and_add(ctx, arr, idx, &delta)));
    }

    // Resolve kind + field via the meta side-table FIRST (real-JDK VarHandles —
    // e.g. `java.net.Socket.STATE` — do NOT carry our synthetic 6-field layout,
    // so reading `VH_KIND` off the object yields garbage and the update silently
    // no-ops returning 0). Fall back to the synthetic fields. Mirrors
    // `varhandle_get_and_bitwise` / `varhandle_compare_and_set`.
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Int(0))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_ARRAY => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Round 15 wave 3 (lane vh4): checked, and only for an array.
            let array_class = meta.as_deref().map(|m| m.class_name.as_str());
            let Some(idx) = vh_kind_array_fallback_index(ctx, arr, array_class, args)? else {
                return Ok(Some(Value::Int(0)));
            };
            let delta = args.get(3).cloned().unwrap_or(Value::Int(0));
            // LOW-finding fix: atomic via the shared CAS retry helper.
            Ok(Some(array_get_and_add(ctx, arr, idx, &delta)))
        }
        VH_KIND_INSTANCE => {
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Receiver only: the delta is numeric, so there is no reference to
            // judge and `vh_instance_refusal` is passed `None` for the value.
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, None) {
                return Err(refusal);
            }
            let delta = args.get(2).cloned().unwrap_or(Value::Int(0));
            let idx = if field_idx >= 0 {
                field_idx as usize
            } else {
                // By name in the field's holder, not the coordinate.
                match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    Some(i) => {
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                        i
                    }
                    None => return Ok(Some(Value::Int(0))),
                }
            };
            // LOW-finding fix: atomic instance getAndAdd via a bounded CAS
            // retry loop (was a non-atomic get/add/set → lost updates under
            // contention). The sum is recomputed from the freshly-read `old`
            // each iteration so a racing update is never clobbered.
            // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`).
            // t1 wave 9, lane ATOM2 (page t1-rlatom-vh-getandadd-without-a-row-
            // keeps-the-sub-word-carry): a row-less handle narrows by its own
            // `VH_FIELD_DESC` slot (`vh_field_desc`), not by `""`.
            let var_desc_owned: Cow<'_, str> = match meta.as_deref() {
                Some(m) => Cow::Borrowed(m.field_desc.as_str()),
                None => vh_field_desc(ctx, this),
            };
            let var_desc: &str = &var_desc_owned;
            let old = vh_cas_rmw(ctx, receiver, idx, VhRmwSlot::Field, |old| {
                add_values_narrowed(old, &delta, var_desc)
            });
            Ok(Some(old))
        }
        VH_KIND_STATIC => {
            let delta = args.get(1).cloned().unwrap_or(Value::Int(0));
            // `delta` is numeric: nothing to keep alive across `<clinit>`.
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            if let Some((cid, sidx)) = vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut [],
                vh_static_init(mode, meta.as_deref()),
            )? {
                // LOW-finding fix: atomic static getAndAdd — serialize the
                // get/add/set on the per-class mirror monitor (no CAS
                // primitive exists for static slots).
                // t1 wave 9, lane ATOM2: a row-less handle narrows by its
                // `VH_FIELD_DESC` slot (see the instance arm).
                let var_desc_owned: Cow<'_, str> = match meta.as_deref() {
                    Some(m) => Cow::Borrowed(m.field_desc.as_str()),
                    None => vh_field_desc(ctx, this),
                };
                let var_desc: &str = &var_desc_owned;
                // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`): a
                // hardware CAS loop on the static cell first.
                if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |old| {
                    Some(add_values_narrowed(&old, &delta, var_desc))
                }) {
                    return Ok(Some(old));
                }
                let old = vh_with_static_lock(ctx, cid, &[], |ctx, _| {
                    // JIT round 22 wave 5c, lane sync: the hardware arm again
                    // under the lock (see the CAS arm).
                    if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |old| {
                        Some(add_values_narrowed(&old, &delta, var_desc))
                    }) {
                        return old;
                    }
                    let old = ctx.get_static_field(cid, sidx);
                    let new_val = add_values_narrowed(&old, &delta, var_desc);
                    ctx.set_static_field(cid, sidx, new_val);
                    old
                });
                Ok(Some(old))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

/// Bitwise atomic op selector for `VarHandle.getAndBitwise{Or,And,Xor}`.
#[derive(Copy, Clone)]
pub enum VhBitOp {
    Or,
    And,
    Xor,
}

/// `VarHandle.getAndBitwise{Or,And,Xor}(receiver, mask) -> old value`.
///
/// Signature-polymorphic, same arg shapes as [`varhandle_get_and_add`]
/// (array / instance / static). Needed by `java.net.Socket`, whose `state`
/// field is updated with `STATE.getAndBitwiseOr(this, flag)` during
/// connect/accept; without it the real-JDK Socket path throws
/// `NoSuchMethodError`. See `reference_server_socket_gap`.
///
/// Boxes through [`vh_box_access_result`] — see there for why the erased
/// `Object` call site needs it and why the boxing cannot live at the
/// poly-return boundary.
pub fn varhandle_get_and_bitwise(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: VhBitOp,
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    vh_check_access_mode_supported(ctx, args, VhModeFamily::Bitwise, mode)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    let kind = vh_check_arity_row(ctx, args, 4, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::Bitwise(op), mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::Bitwise(op), mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::Bitwise(op))
    } else {
        varhandle_get_and_bitwise_raw(ctx, args, op, &row, mode)
    };
    vh_box_access_result(ctx, args, raw)
}

pub fn varhandle_get_and_bitwise_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: VhBitOp,
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    fn apply(current: &Value, mask: &Value, op: VhBitOp) -> Value {
        let f = |a: i64, b: i64| match op {
            VhBitOp::Or => a | b,
            VhBitOp::And => a & b,
            VhBitOp::Xor => a ^ b,
        };
        match (current, mask) {
            (Value::Int(a), Value::Int(b)) => Value::Int(f(*a as i64, *b as i64) as i32),
            (Value::Long(a), Value::Long(b)) => Value::Long(f(*a, *b)),
            (Value::Long(a), Value::Int(b)) => Value::Long(f(*a, *b as i64)),
            (Value::Int(a), Value::Long(b)) => Value::Int(f(*a as i64, *b) as i32),
            _ => current.clone(),
        }
    }

    // LOW-finding fix: atomic getAndBitwise* on an array element via a bounded
    // CAS retry loop (was a non-atomic get/apply/set → lost updates under
    // contention). Same shape as the getAndAdd array helper; the masked value
    // is recomputed from the freshly-read `old` each iteration.
    fn array_get_and_bitwise(
        ctx: &mut dyn NativeContext,
        arr: ObjectRef,
        idx: usize,
        mask: &Value,
        op: VhBitOp,
    ) -> Value {
        // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`).
        vh_cas_rmw(ctx, arr, idx, VhRmwSlot::ArrayElement, |old| {
            apply(old, mask, op)
        })
    }

    // Array-element form — args = [vh, array, idx, mask].
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let mask = args.get(3).cloned().unwrap_or(Value::Int(0));
        return Ok(Some(array_get_and_bitwise(ctx, arr, idx, &mask, op)));
    }

    // Resolve kind + field via the meta side-table FIRST (real-JDK VarHandles —
    // e.g. `java.net.Socket.STATE` — do NOT carry our synthetic 6-field layout,
    // so reading `VH_KIND` off the object yields garbage). Fall back to the
    // synthetic fields. Mirrors `varhandle_compare_and_set`.
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Int(0))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_ARRAY => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Round 15 wave 3 (lane vh4): checked, and only for an array.
            let array_class = meta.as_deref().map(|m| m.class_name.as_str());
            let Some(idx) = vh_kind_array_fallback_index(ctx, arr, array_class, args)? else {
                return Ok(Some(Value::Int(0)));
            };
            let mask = args.get(3).cloned().unwrap_or(Value::Int(0));
            // LOW-finding fix: atomic via the shared CAS retry helper.
            Ok(Some(array_get_and_bitwise(ctx, arr, idx, &mask, op)))
        }
        VH_KIND_INSTANCE => {
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Receiver only -- the mask is numeric. See `getAndAdd` above.
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, None) {
                return Err(refusal);
            }
            let mask = args.get(2).cloned().unwrap_or(Value::Int(0));
            let idx = if field_idx >= 0 {
                field_idx as usize
            } else {
                // By name in the field's holder, not the coordinate.
                match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    Some(i) => {
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                        i
                    }
                    None => return Ok(Some(Value::Int(0))),
                }
            };
            // LOW-finding fix: atomic instance getAndBitwise* via a bounded CAS
            // retry loop (was a non-atomic get/apply/set → lost updates under
            // contention). The masked value is recomputed from the
            // freshly-read `old` each iteration so a racing update is kept.
            // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`).
            let old = vh_cas_rmw(ctx, receiver, idx, VhRmwSlot::Field, |old| {
                apply(old, &mask, op)
            });
            Ok(Some(old))
        }
        VH_KIND_STATIC => {
            let mask = args.get(1).cloned().unwrap_or(Value::Int(0));
            // `mask` is integral: nothing to keep alive across `<clinit>`.
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            if let Some((cid, sidx)) = vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut [],
                vh_static_init(mode, meta.as_deref()),
            )? {
                // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`): a
                // hardware CAS loop on the static cell first.
                if let Some((old, _)) =
                    ctx.static_field_hw_update(cid, sidx, &mut |old| Some(apply(&old, &mask, op)))
                {
                    return Ok(Some(old));
                }
                // LOW-finding fix: atomic static getAndBitwise* — serialize the
                // get/apply/set on the per-class mirror monitor (no CAS
                // primitive exists for static slots).
                let old = vh_with_static_lock(ctx, cid, &[], |ctx, _| {
                    // JIT round 22 wave 5c, lane sync: the hardware arm again
                    // under the lock (see the CAS arm).
                    if let Some((old, _)) =
                        ctx.static_field_hw_update(cid, sidx, &mut |old| Some(apply(&old, &mask, op)))
                    {
                        return old;
                    }
                    let old = ctx.get_static_field(cid, sidx);
                    let new_val = apply(&old, &mask, op);
                    ctx.set_static_field(cid, sidx, new_val);
                    old
                });
                Ok(Some(old))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

pub fn varhandle_get_and_bitwise_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOr")
}
pub fn varhandle_get_and_bitwise_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAnd")
}
pub fn varhandle_get_and_bitwise_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXor")
}

