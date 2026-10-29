// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// =============================================================================
// java.lang.invoke.CallSite expansion — MutableCallSite, ConstantCallSite, VolatileCallSite
// CallSite = 1-field synthetic (target=0 MethodHandle)
// =============================================================================

/// Resolve the heap slot holding a `CallSite`'s `target` `MethodHandle`.
///
/// Two layouts reach these natives and they do NOT agree on slot 0:
///   * real JDK `java.lang.invoke.CallSite` — `{ type, target, context }`, so
///     `target` is NOT slot 0 (slot 0 is the `MethodType`);
///   * the synthetic 1-field CallSite model minted by `ensure_synthetic_class`,
///     whose fields are UNNAMED, so only slot 0 is addressable.
///
/// Reading/writing a hard-coded slot 0 in one native and the *named* `target`
/// field in another (which is exactly what `getTarget` here and the
/// `register_method_handle_combinator_extras_bridge` copy of
/// `MutableCallSite.setTarget` used to do) makes the writer and the reader
/// address different storage: on the synthetic layout `set_field_by_name`
/// silently no-ops, so `setTarget(mh2); getTarget()` handed back the ctor's
/// original target. Every CallSite native below funnels through this one
/// resolver so writer and reader can never disagree again.
pub(crate) fn cs_target_slot(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let cid = ctx.class_id_of_object(this);
    ctx.resolve_field_index_by_class_id(cid, "target")
        .unwrap_or(0)
}

/// Read a call site's current `target` (see [`cs_target_slot`]).
pub(crate) fn cs_read_target(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let slot = cs_target_slot(ctx, this);
    ctx.get_field(this, slot)
}

/// Store a call site's `target` (see [`cs_target_slot`]).
pub(crate) fn cs_write_target(ctx: &mut dyn NativeContext, this: ObjectRef, target: Value) {
    let slot = cs_target_slot(ctx, this);
    ctx.set_field(this, slot, target);
}

/// The `type()` of a call-site target, when it can be trusted: one of OUR
/// handles (wide enough to carry `MH_KIND`) of a kind whose `type` field is
/// the adapted signature ([`kind_has_authoritative_adapted_type`], plus
/// `constant` / `identity` / `zero` / `empty` and the uninitialized call site's
/// placeholder, all `MH_KIND_CONSTANT` / `MH_KIND_IDENTITY` minted with their
/// exact type). `None` is unknown.
pub(crate) fn cs_target_type(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if ctx.object_num_fields(mh) <= MH_KIND {
        // SBR4 mhcons: a REAL JDK handle (what the retired `Lookup.find*` and the
        // JDK's own combinators build) has no synthetic slots, and its `type` field
        // IS its `MethodType`. Answering "unknown" here let `setTarget` accept a
        // real handle of another type (HotSpot: `WrongMethodTypeException`, the
        // check `CallSite.checkTargetChange` makes); the descriptor comparison in
        // `cs_set_target_refusal` is the one carriers get. A stub with no `type`
        // still answers `None`.
        return mh_declared_descriptor(ctx, mh);
    }
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => return None,
    };
    if !kind_has_authoritative_adapted_type(kind)
        && !matches!(kind, MH_KIND_CONSTANT | MH_KIND_IDENTITY | MH_KIND_THROW)
    {
        return None;
    }
    mh_declared_descriptor(ctx, mh)
}

/// What `CallSite.checkTargetChange` raises for `setTarget(new_target)` on the
/// Mutable / Volatile call site `this`, or `None` (interpreter round i1 wave
/// 24, lane L4; page
/// `interpreter-L4-callsite-settarget-and-asfixedarity-diverge-from-hotspot-FIXED-20260927.md`).
///
/// The natives used to store any target, so a `()int` site accepted a
/// `()String` handle and its invoker then read a String as an int. HotSpot:
/// a null target is the helpful NPE of `newTarget.type()`, and a target of
/// another type is `WrongMethodTypeException: MethodHandle<new> should be of
/// type <old>` (`CallSite.wrongTargetType`), measured by
/// `tools/probes/interp/L4/L4W24CallSiteAndArity.java`. The type comparison
/// is by DESCRIPTOR (a fresh `MethodType` per call defeats the JDK's
/// reference equality), and refuses only when both types are known
/// ([`cs_target_type`]) and neither is the fabricated `()V` stand-in
/// `finish_method_handle` writes for "unknown" -- every way of not knowing is
/// an accept, as in `exact_call_site_refusal`.
pub(crate) fn cs_set_target_refusal(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_target: Value,
) -> Option<MethodCallFailed> {
    let Value::Object(Some(new_target)) = new_target else {
        return Some(
            RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"newTarget\" is null"
                        .to_string(),
                ),
            }
            .into(),
        );
    };
    if !CS_SET_TARGET_CHECKS_TYPE {
        return None;
    }
    let Value::Object(Some(old)) = cs_read_target(ctx, this) else {
        return None;
    };
    let new_desc = cs_target_type(ctx, new_target)?;
    let old_desc = cs_target_type(ctx, old)?;
    if new_desc == old_desc || new_desc == "()V" || old_desc == "()V" {
        return None;
    }
    let new_shown = method_type_display_in(&*ctx, &new_desc)?;
    let old_shown = method_type_display_in(&*ctx, &old_desc)?;
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        &format!("MethodHandle{new_shown} should be of type {old_shown}"),
    ))
}

/// Kill switch for [`cs_set_target_refusal`]'s TYPE comparison (wave 24).
/// Its guards are Groovy's `IndyInterface` (`probes/CodegenFrameworkSmoke.java`)
/// and the regression suite's `RJdkHandles`, which re-target call sites with
/// adapter chains; `false` restores the store-anything behaviour (the null
/// refusal stays).
pub(crate) const CS_SET_TARGET_CHECKS_TYPE: bool = true;

/// `MutableCallSite` / `VolatileCallSite` / `CallSite.setTarget`: check, then
/// store (see [`cs_set_target_refusal`]).
pub(crate) fn cs_set_target(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_target = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some(refusal) = cs_set_target_refusal(ctx, this, new_target) {
        return Err(refusal);
    }
    cs_write_target(ctx, this, new_target);
    Ok(None)
}

/// `static native void MethodHandleNatives.setCallSiteTargetNormal(CallSite site,
/// MethodHandle target)` and its `...Volatile` sibling: the VM half of
/// `CallSite.setTargetNormal` / `setTargetVolatile`, which `MutableCallSite.
/// setTarget` and `VolatileCallSite.setTarget` reach in JDK bytecode (the
/// `checkTargetChange` type check has already run in Java). HotSpot stores the
/// reference into `CallSite.target` and deoptimizes the code that depended on
/// the old one.
///
/// Here there is no dependency list to flush: an `invokedynamic` linked to a
/// non-constant call site (the interpreter's generic row, the JIT's `Linked`
/// row and every `dynamicInvoker`) reads `CallSite.target` per execution
/// (`vm/src/runtime/invokedynamic.rs`, `cs_target_slot`'s field). The JIT does
/// bind a `MutableCallSite` / `VolatileCallSite` target since JIT round 20
/// wave 11 (`CRATONVM_JIT_INDY_MUTABLE_GUARD`), but only behind a guard that
/// re-reads this field on every call and compares it with the planned handle
/// (`IrBuilder::try_indy_mutable_guarded_call`), and the bridge's direct memo
/// re-validates the same identity per call (`jit_indy_linked_direct_leaf`);
/// a retarget is therefore seen at the next call with nothing to invalidate
/// (`CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE` only re-plans which target
/// the guard expects). So the store through [`cs_write_target`] -- the one
/// resolver every other call site native here uses -- IS the whole retarget;
/// `volatile` adds the full fence `putReferenceVolatile` gives.
///
/// Since the 2026-10-11 FFM merge concrete JDK bytecode beats the Bridges that
/// used to shadow `MutableCallSite.setTarget`, so the JDK body now runs and
/// reached these two bare natives, which nothing registered (Groovy script
/// evaluation: `UnsatisfiedLinkError: ... setCallSiteTargetNormal`).
///
/// Both rows run [`native_mhn_set_call_site_target`] (JIT round 20, lane
/// dispatch, which fixed the same `UnsatisfiedLinkError`): a null call site is
/// the JVM's `NullPointerException`, and the volatile form fences on both
/// sides of the store.
pub(crate) fn mhn_set_call_site_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    volatile: bool,
) -> MethodCallResult {
    native_mhn_set_call_site_target(ctx, args, volatile)
}

pub(crate) fn native_mhn_set_call_site_target_normal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    mhn_set_call_site_target(ctx, args, false)
}

pub(crate) fn native_mhn_set_call_site_target_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    mhn_set_call_site_target(ctx, args, true)
}

pub(crate) fn register_p60_callsite(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // CallSite base
    let cs = "java/lang/invoke/CallSite";
    r.register(
        cs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(cs_read_target(ctx, this)))
        },
    );
    r.register(
        cs,
        "setTarget",
        "(Ljava/lang/invoke/MethodHandle;)V",
        cs_set_target,
    );
    // `CallSite.type()` is defined as `target.type()`. It used to be a constant
    // null, which NPE'd every caller that asks a call site for its type
    // (`CallSite.dynamicInvoker`, `MutableCallSite.syncAll`, and any JDK code
    // that type-checks a call site before invoking it). The target slot is
    // resolved by `cs_target_slot`; a MethodHandle keeps its MethodType in its
    // named `type` field, falling back to slot 0 (see `MethodHandle.type`).
    r.register(
        cs,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let target = match cs_read_target(ctx, this) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            match ctx.get_field_by_name(target, "type") {
                named @ Value::Object(Some(_)) => Ok(Some(named)),
                _ => Ok(Some(ctx.get_field(target, 0))),
            }
        },
    );

    // MutableCallSite
    let mcs = "java/lang/invoke/MutableCallSite";
    r.register(
        mcs,
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cs_write_target(
                ctx,
                this,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(None)
        },
    );
    r.register(
        mcs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(cs_read_target(ctx, this)))
        },
    );
    r.register(
        mcs,
        "setTarget",
        "(Ljava/lang/invoke/MethodHandle;)V",
        cs_set_target,
    );

    // ConstantCallSite
    let ccs = "java/lang/invoke/ConstantCallSite";
    r.register(
        ccs,
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cs_write_target(
                ctx,
                this,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(None)
        },
    );
    r.register(
        ccs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(cs_read_target(ctx, this)))
        },
    );
    r.register(
        ccs,
        "dynamicInvoker",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(cs_read_target(ctx, this)))
        },
    );

    // VolatileCallSite
    let vcs = "java/lang/invoke/VolatileCallSite";
    r.register(
        vcs,
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cs_write_target(
                ctx,
                this,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(None)
        },
    );
    r.register(
        vcs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(cs_read_target(ctx, this)))
        },
    );
    r.register(
        vcs,
        "setTarget",
        "(Ljava/lang/invoke/MethodHandle;)V",
        cs_set_target,
    );
    r.set_category(__prev_cat);
}

// java.lang.invoke.MethodHandles.Lookup — factory + lookup methods
// Lookup = 2-field (lookupClass=0, allowedModes=1)
// =============================================================================

/// The slot the LEGACY synthetic `Lookup` layout puts `allowedModes` in
/// (`lookupClass`=0, `allowedModes`=1). On the real JDK layout slot 1 is
/// `prevLookupClass` — see [`lk_allowed_modes_slot`].
pub(crate) const LK_SYNTHETIC_ALLOWED_MODES: usize = 1;

/// Where `Lookup.in` moves, as far as modules go (JDK 25 compares
/// `lookupClass.getModule()` with the target's).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LkModuleMove {
    /// Both classes are in one module.
    Same,
    /// Another module (the "teleport"); `third` when the lookup had already
    /// teleported (`prevLookupClass != null`) and that class is not in the
    /// target's module either, a hop JDK 25 refuses all access for.
    Across { third: bool },
}

/// [`LkModuleMove`] for `Lookup.in`, or `None` when this context cannot tell
/// (no module registry, a mirror it cannot read): `Lookup.in` then keeps the
/// package-based reductions it always had. A class of the unnamed module is
/// keyed by its defining loader (each loader has its own unnamed module).
/// Interpreter round i1 wave 43, lane L4 (probe
/// `tools/probes/interp/L4/L4W43LookupTeleport.java`).
pub(crate) fn lk_in_teleport(
    ctx: &dyn NativeContext,
    lookup_class: Value,
    plc: Value,
    target: Value,
) -> Option<LkModuleMove> {
    if !ctx.module_is_registered("java.base") {
        return None;
    }
    let by_identity = lookup_in_module_by_identity_enabled();
    if lk_same_module(ctx, by_identity, lookup_class, target)? {
        return Some(LkModuleMove::Same);
    }
    let third = match plc {
        Value::Object(Some(_)) => !lk_same_module(ctx, by_identity, plc, target)?,
        _ => false,
    };
    Some(LkModuleMove::Across { third })
}

/// `CRATONVM_LOOKUP_IN_MODULE_BY_IDENTITY`: `Lookup.in`'s module questions
/// (is the target in the lookup class's module; does one read the other and
/// export the target's package to it) are asked of the VM's module registry by
/// the modules' identities (`NativeClassAccess::module_access`,
/// `cratonvm_classloading::access_control::module_access`) before the
/// name-keyed [`lk_module_key`] answer, which calls every class of a user
/// `ModuleLayer` its loader's unnamed module. Read once per question group (a
/// `Lookup.in` is not a per-bytecode path). Default ON; `=0` restores the
/// name keys. Interpreter round i1 wave 48, lane L6
/// (`i44-L5-proposal-a-classs-module-is-an-identity-every-reader-sees`, the
/// `lk_module_key` reader).
pub(crate) fn lookup_in_module_by_identity_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LOOKUP_IN_MODULE_BY_IDENTITY")
}

/// `CRATONVM_MODULE_EXPORT_ALL_BY_IDENTITY`: "does this class's module export
/// its package to every module" (`Module.isExported(pn)`) is asked of the
/// VM's module registry by the module's identity
/// (`ModuleQuestion::ExportAll`) before the name-keyed answer, which reads a
/// class of a user `ModuleLayer` as its loader's unnamed module and so every
/// one of its packages as exported. Readers: `Lookup.in` within one module
/// without `MODULE` (below) and `Proxy`'s `nonExported`
/// (`reflect_annotations.rs` `proxy_interfaces_non_exported`). Read per
/// question (neither is a per-bytecode path). Interpreter round i1 wave 49,
/// lane L6 (`interpreter-L6-proposal-an-unqualified-export-question-by-identity-FIXED-20261013.md`).
///
/// Since interpreter round i6 wave 118 (lane L5) the HotSpot-alignment row
/// `module-export-all-by-identity` (`HotSpotAlignment::ModuleExportAllByIdentity`,
/// default on, the variable its legacy key): in force under `--jdk-only`
/// whatever the variable says, per VM.
pub(crate) fn export_all_by_identity_enabled(ctx: &dyn NativeContext) -> bool {
    ctx.aligned(cratonvm_types::HotSpotAlignment::ModuleExportAllByIdentity)
}

/// The one module question ([`cratonvm_native_api::ModuleQuestion`]) about
/// the modules of the classes the mirrors `accessor` and `target` denote, by
/// identity: `None` when a mirror cannot be read or the registry holds no
/// answer (the caller keeps its name-keyed answer). A registry answer is
/// traced under `CRATONVM_DBG=access` (the positive control).
pub(crate) fn lk_module_question(
    ctx: &dyn NativeContext,
    accessor: Value,
    target: Value,
    question: cratonvm_native_api::ModuleQuestion,
) -> Option<bool> {
    let (Value::Object(Some(a)), Value::Object(Some(t))) = (accessor, target) else {
        return None;
    };
    let a = crate::lang_class::mirror_class_id(ctx, a)?;
    let t = crate::lang_class::mirror_class_id(ctx, t)?;
    let answer = ctx.module_access(a, t, question)?;
    if crate::jboss_jdkspecific::dbg_layer_modules() {
        eprintln!(
            "[ACCESS-DBG] Lookup.in by identity: {} -> {} {question:?}={answer}",
            ctx.class_name_of_id(a).unwrap_or_default(),
            ctx.class_name_of_id(t).unwrap_or_default(),
        );
    }
    Some(answer)
}

/// Are the classes `a` and `b` denote in one module? By identity under
/// [`lookup_in_module_by_identity_enabled`], else (or when the registry cannot
/// answer) by [`lk_module_key`]. `None` when a mirror cannot be read.
pub(crate) fn lk_same_module(ctx: &dyn NativeContext, by_identity: bool, a: Value, b: Value) -> Option<bool> {
    if by_identity {
        if let Some(same) = lk_module_question(ctx, a, b, cratonvm_native_api::ModuleQuestion::Same)
        {
            return Some(same);
        }
    }
    Some(lk_module_key(ctx, a)? == lk_module_key(ctx, b)?)
}

/// `VerifyAccess.isModuleAccessible` for the module of the class `reader`
/// denotes and the module of `target` (its package): by identity (READ, then
/// EXPORT) under [`lookup_in_module_by_identity_enabled`], else (or when the
/// registry cannot answer) [`lk_module_accessible`] over the name keys.
pub(crate) fn lk_module_accessible_to(
    ctx: &dyn NativeContext,
    by_identity: bool,
    reader: Value,
    target: Value,
    pkg: &str,
) -> Option<bool> {
    use cratonvm_native_api::ModuleQuestion;
    if by_identity {
        match lk_module_question(ctx, reader, target, ModuleQuestion::Read) {
            Some(false) => return Some(false),
            Some(true) => {
                if let Some(exported) =
                    lk_module_question(ctx, reader, target, ModuleQuestion::Export)
                {
                    return Some(exported);
                }
            }
            None => {}
        }
    }
    // Interpreter round i7 wave 136, lane L4 (proposal
    // `i45-L5-proposal-one-module-access-question-by-identity`, wave-125 table
    // row 1, the reverse-direction readers; under
    // `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`, default off): a READER of a
    // user `ModuleLayer` is asked by its identity, not by the name key, which
    // reads it as its loader's unnamed module (every qualified export missed)
    // or as the boot layer's module of that name.
    if cratonvm_types::flags().loader.module_access_export_one_question {
        if let Some(answer) = lk_layer_reader_accessible(ctx, reader, target, pkg) {
            return Some(answer);
        }
    }
    let reader = lk_module_key(ctx, reader)?;
    let owner = lk_module_key(ctx, target)?;
    Some(lk_module_accessible(ctx, &reader, &owner, pkg))
}

/// [`lk_module_accessible_to`] for a reader class of a user `ModuleLayer`
/// module and a target class of a name-map module, by the reader's identity
/// (interpreter round i7 wave 136, lane L4): READ by identity
/// (`ModuleQuestion::Read`, else the name-keyed reads edge, which is how reads
/// are recorded), then `pkg` exported to THAT layer module
/// (`is_package_exported_or_open_to_layer_module` with its loader namespace:
/// a descriptor's `exports p to m` never reaches a layer's `m`, and under the
/// switch a run-time edge reaches only the module it was recorded to). `None`
/// for any other pair (the caller keeps its answer).
pub(crate) fn lk_layer_reader_accessible(
    ctx: &dyn NativeContext,
    reader: Value,
    target: Value,
    pkg: &str,
) -> Option<bool> {
    use cratonvm_native_api::{ClassModuleIdentity, ModuleQuestion};
    let (Value::Object(Some(r)), Value::Object(Some(t))) = (reader, target) else {
        return None;
    };
    let r = crate::lang_class::mirror_class_id(ctx, r)?;
    let t = crate::lang_class::mirror_class_id(ctx, t)?;
    let ClassModuleIdentity::Layer {
        loader_ns,
        name: reader_name,
        ..
    } = ctx.module_identity_of_class(r)
    else {
        return None;
    };
    let ClassModuleIdentity::Named(owner) = ctx.module_identity_of_class(t) else {
        return None;
    };
    let reads = ctx
        .module_access(r, t, ModuleQuestion::Read)
        .unwrap_or_else(|| ctx.reads_module(&reader_name, &owner));
    if !reads {
        return Some(false);
    }
    ctx.is_package_exported_or_open_to_layer_module(
        &owner,
        pkg,
        &reader_name,
        Some(loader_ns),
        false,
    )
}

/// The module of the class a mirror denotes: its name, or `None` and its
/// defining loader for a class of an unnamed module (one per loader). `None`
/// when the mirror cannot be read.
pub(crate) fn lk_module_key(ctx: &dyn NativeContext, v: Value) -> Option<(Option<String>, i32)> {
    match v {
        Value::Object(Some(m)) => crate::lang_class::mirror_class_id(ctx, m).map(|cid| {
            match ctx.module_name_of_class(cid) {
                Some(name) => (Some(name), 0),
                None => (None, ctx.loader_id_of_class(cid)),
            }
        }),
        _ => None,
    }
}

/// Can module `reader` use the public classes of package `pkg` (internal,
/// `/`-separated) of module `owner`: `reader` reads `owner` and `owner`
/// exports `pkg` to it (`VerifyAccess.isModuleAccessible`, one side). An
/// unnamed module reads every module; a named one reads no unnamed module; an
/// unnamed module exports every package.
pub(crate) fn lk_module_accessible(
    ctx: &dyn NativeContext,
    reader: &(Option<String>, i32),
    owner: &(Option<String>, i32),
    pkg: &str,
) -> bool {
    if reader == owner {
        return true;
    }
    let reads = match (&reader.0, &owner.0) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(r), Some(o)) => ctx.reads_module(r, o),
    };
    if !reads {
        return false;
    }
    match (&owner.0, &reader.0) {
        (None, _) => true,
        (Some(o), Some(r)) => {
            ctx.is_package_exported_unqualified(o, pkg) || ctx.is_package_exported_to(o, pkg, r)
        }
        (Some(o), None) => ctx.is_package_exported_unqualified(o, pkg),
    }
}

/// `VerifyAccess.isClassAccessible(target, lookupClass, prevLookupClass,
/// prev)` (JDK 25), with module readability and exports read from the
/// module registry; `lookup_class` and `old_plc` are the RECEIVER's lookup
/// class and previous lookup class, `prev` its modes. `None` when a module or
/// the target's package cannot be read.
pub(crate) fn lk_class_accessible_in_modules(
    ctx: &dyn NativeContext,
    prev: i32,
    same_package: bool,
    target_is_public: bool,
    lookup_class: Value,
    old_plc: Value,
    target: Value,
) -> Option<bool> {
    if prev == 0 {
        return Some(false);
    }
    if prev & LK_MODE_PACKAGE != 0 && same_package {
        return Some(true);
    }
    if !target_is_public {
        return Some(false);
    }
    let by_identity = lookup_in_module_by_identity_enabled();
    let ref_module = lk_module_key(ctx, target)?;
    let has_plc = matches!(old_plc, Value::Object(Some(_)));
    let pkg = match target {
        Value::Object(Some(m)) => {
            let name = crate::lang_class::mirror_class_name(ctx, m)?;
            name.rsplit_once('/')
                .map(|(p, _)| p.to_string())
                .unwrap_or_default()
        }
        _ => return None,
    };
    if !has_plc && lk_same_module(ctx, by_identity, lookup_class, target)? {
        if prev & LK_MODE_MODULE != 0 {
            return Some(true);
        }
        // `refModule.isExported(pn)`, by the module's identity under
        // `CRATONVM_MODULE_EXPORT_ALL_BY_IDENTITY`: a layer module has no name
        // here, so its qualified-only package read as exported to all. Only
        // for a nameless module; a named one keeps the name-map answer below.
        if ref_module.0.is_none() && export_all_by_identity_enabled(ctx) {
            if let Some(exported) = lk_module_question(
                ctx,
                target,
                target,
                cratonvm_native_api::ModuleQuestion::ExportAll,
            ) {
                return Some(exported);
            }
        }
        // `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` (default off;
        // interpreter round i6 wave 119, lane L5): a named module too by its
        // identity, when the registry answers.
        if cratonvm_types::flags().loader.module_access_export_one_question {
            if let Some(exported) = lk_module_question(
                ctx,
                target,
                target,
                cratonvm_native_api::ModuleQuestion::ExportAll,
            ) {
                return Some(exported);
            }
        }
        return Some(match &ref_module.0 {
            None => true,
            Some(o) => ctx.is_package_exported_unqualified(o, &pkg),
        });
    }
    let second_ok = if has_plc {
        lk_module_accessible_to(ctx, by_identity, old_plc, target, &pkg)?
    } else {
        true
    };
    Some(lk_module_accessible_to(ctx, by_identity, lookup_class, target, &pkg)? && second_ok)
}

/// `Lookup.in`'s modes within ONE module (JDK 25): the full-power bits less
/// `ORIGINAL`; `PACKAGE`, `PRIVATE` and `PROTECTED` dropped for another
/// package, `PRIVATE` and `PROTECTED` for a class outside the nest; then
/// `VerifyAccess.isClassAccessible` (`accessible`). `MODULE` is KEPT:
/// measured, `lookup().in(<a public class of another package of the same
/// module>)` is `17` (`PUBLIC|MODULE`), where the package-based reduction
/// answered `1`.
pub(crate) fn lk_in_modes_same_module(
    prev: i32,
    same_package: bool,
    same_nest: bool,
    accessible: bool,
) -> i32 {
    let mut modes = prev & LK_MODE_FULL_POWER;
    if modes & LK_MODE_PACKAGE != 0 && !same_package {
        modes &= !(LK_MODE_PACKAGE | LK_MODE_PRIVATE | LK_MODE_PROTECTED);
    }
    if modes & LK_MODE_PRIVATE != 0 && !same_nest {
        modes &= !(LK_MODE_PRIVATE | LK_MODE_PROTECTED);
    }
    if modes & LK_MODE_PUBLIC != 0 && !accessible {
        modes = 0;
    }
    modes
}

/// `Lookup.in`'s modes for a move to another module (JDK 25): `ORIGINAL`
/// dropped with the rest of the non-full-power bits, `0` for a hop to a third
/// module, then `MODULE`, `PACKAGE`, `PRIVATE` and `PROTECTED` dropped, then
/// `VerifyAccess.isClassAccessible` (`accessible`). Measured:
/// `lookup().in(String.class)` is `1`, its `.in(java.sql.Connection.class)`
/// is `0`, and its `.in(<the original lookup class>)` is `0` (`java.base`
/// does not read the unnamed module).
pub(crate) fn lk_in_modes_across_modules(prev: i32, third_module: bool, accessible: bool) -> i32 {
    let mut modes = if third_module {
        0
    } else {
        prev & LK_MODE_FULL_POWER
    };
    modes &= !(LK_MODE_MODULE | LK_MODE_PACKAGE | LK_MODE_PRIVATE | LK_MODE_PROTECTED);
    if modes & LK_MODE_PUBLIC != 0 && !accessible {
        modes = 0;
    }
    modes
}

/// `VerifyAccess.isClassAccessible(target, lookupClass, prevLookupClass,
/// prev)` as far as this model can answer it: a public class is accessible
/// (module readability and exports are not modelled here), any other only to
/// a lookup with `PACKAGE` access in the target's own package. `prev == 0` is
/// never accessible.
pub(crate) fn lk_in_class_accessible(prev: i32, same_package: bool, target_is_public: bool) -> bool {
    if prev == 0 {
        return false;
    }
    (prev & LK_MODE_PACKAGE != 0 && same_package) || target_is_public
}

/// The slot of the DECLARED `allowedModes` field, or `None` when the receiver
/// does not carry the real `java.lang.invoke.MethodHandles$Lookup` layout.
///
/// The witness is CLASS-side, and that is the whole point. `get_field_by_name`
/// answers `Int(0)` for an ABSENT field, which is indistinguishable from a
/// genuine `allowedModes == 0` — and a FABRICATED Lookup stub has no
/// `allowedModes` at all, it names its slots `_f0..`. The previous
/// "by name first, synthetic slot second" reader therefore returned 0 for
/// every synthetic Lookup and NEVER reached the slot-1 fallback below it: that
/// fallback was unreachable code.
///
/// Asking the class separates "absent" from "present and zero" with no
/// ambiguity, and is descriptor-safe — `resolve_field_index_by_class_id`
/// resolves the declared `int allowedModes` on `MethodHandles$Lookup` rather
/// than a same-named field of some other type in the hierarchy.
///
/// Measured layout (`javap -p java.lang.invoke.MethodHandles$Lookup`, OpenJDK
/// 25.0.3): `lookupClass`(0), `prevLookupClass`(1), `allowedModes`(2),
/// `cachedProtectionDomain`(3). Slot 1 is a REFERENCE.
///
/// This duplicates `classloader::lk_real_allowed_modes_slot` /
/// `classloader::lk_modes_of`, which are private to that module. Collapsing
/// the two needs a one-line `pub(crate)` on `classloader::lk_modes_of`; see
/// the lane report.
/// Pin the reference inside a `Value` across an allocation, if it has one.
///
/// Every `MethodHandles$Lookup` factory in this registrar has the same shape:
/// resolve a class mirror, THEN allocate the Lookup, THEN store the mirror into
/// its `lookupClass`. The allocation can complete a moving young collection and
/// the mirror is a bare Rust local that nothing rewrites, so the store writes a
/// PRE-GC address into a live object's reference slot.
///
/// `privateLookupIn` has had this fix since the day its own comment was
/// written -- *"`alloc_concurrent_synthetic` can run a moving GC, after which
/// the `target` ObjectRef the VM handed us in `args` is stale ... the old code
/// wrote the pre-GC ref straight into `lookupClass`"* -- and `lookup()`,
/// `publicLookup()` and `Lookup.in` sat three screens away without it. Reached
/// through `MethodHandles.lookup()` from a `<clinit>`, the stale mirror is what
/// `CRATONVM_DBG_STALE_OBJREF` catches and what SIGSEGVs once the evacuated
/// arena is decommitted.
pub(crate) fn lk_pin(ctx: &mut dyn NativeContext, v: Value) -> usize {
    match v {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    }
}

/// Read back what [`lk_pin`] pinned, at its post-collection address.
pub(crate) fn lk_repin_read(ctx: &dyn NativeContext, pin: usize, v: Value) -> Value {
    match v {
        Value::Object(Some(o)) if pin != usize::MAX => {
            Value::Object(Some(ctx.read_native_pin(pin, o)))
        }
        other => other,
    }
}

pub(crate) fn lk_allowed_modes_slot(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "allowedModes")
}

/// Write a `MethodHandles$Lookup`'s `allowedModes` so it lands on the correct
/// field regardless of whether the object carries the **real** JDK 3-field
/// layout (`lookupClass`, `prevLookupClass`, `allowedModes`) or the legacy
/// **synthetic** 2-field layout (`lookupClass`=0, `allowedModes`=1).
///
/// The original code wrote the modes int to fixed slot 1. In the real layout
/// slot 1 is the *reference* field `prevLookupClass`, so `lookupModes()` (real
/// bytecode reading the real `allowedModes` at slot 2) saw 0 — i.e. NO `PACKAGE`
/// access — and Spring CGLIB's `ReflectUtils.defineClass` failed every concrete
/// class proxy with "Lookup does not have PACKAGE access".
///
/// The two arms below are now mutually exclusive by construction, which the
/// "by name, then slot 1 if the read-back disagrees" form was NOT:
///
/// * `modes == 0` on a fabricated Lookup read back as `Int(0)` from the absent
///   field, so the write "landed" and the synthetic slot never got it;
/// * any read-back disagreement on a REAL Lookup fell through to slot 1 and
///   put an `Int` in the `prevLookupClass` reference slot — heap corruption
///   the GC scans as an oop, not merely a wrong answer.
pub(crate) fn lk_write_allowed_modes(ctx: &mut dyn NativeContext, obj: ObjectRef, modes: i32) {
    if let Some(slot) = lk_allowed_modes_slot(ctx, obj) {
        ctx.set_field_by_name(obj, "allowedModes", Value::Int(modes));
        if !matches!(ctx.get_field(obj, slot), Value::Int(m) if m == modes) {
            // Named write did not land; go through the resolved index. Never
            // through the synthetic slot — see the note above.
            ctx.set_field(obj, slot, Value::Int(modes));
        }
        return;
    }
    ctx.set_field(obj, LK_SYNTHETIC_ALLOWED_MODES, Value::Int(modes));
}

/// Read a `Lookup`'s `allowedModes` from whichever layout the receiver has.
///
/// See [`lk_allowed_modes_slot`] for why the class-side witness comes first:
/// the previous by-name-first form answered 0 for every fabricated Lookup
/// (absent field == `Int(0)`), which made `lookupModes()` report 0, made
/// `lk_enforce_find_access` take its "modes unknown, stay permissive" valve,
/// and — compounding with the old `Lookup.in` default — turned that 0 into
/// FULL_POWER on the way out of `in()`.
pub(crate) fn lk_read_allowed_modes(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    lk_read_allowed_modes_opt(ctx, this).unwrap_or(0)
}

/// [`lk_read_allowed_modes`], with "0" and "could not read" kept APART.
///
/// This is the distinction the whole access-control valve rests on.
/// `lk_read_allowed_modes` collapses them to `0`, so every enforcement site had
/// to treat `0` as "unknown, stay permissive" — and that is correct for a
/// Lookup whose layout the VM does not model, but a genuine zero-mode Lookup is
/// one the JDK refuses EVERYTHING, public members included:
///
/// ```text
///   MethodHandles.lookup().dropLookupMode(PUBLIC).lookupModes()  ->  0
///   …that Lookup .findStatic(<a public method of a public class>)
///                                        ->  IllegalAccessException
/// ```
///
/// Measured on OpenJDK 25.0.3. CratonVM admitted it, because the two zeroes
/// were the same value. `Some(0)` is the JDK's zero and is refused; `None` is
/// "this VM could not read the field" and stays permissive, which keeps the
/// one failure mode a new refusal path must not have — turning every Lookup
/// shape the VM does not model into an `IllegalAccessException`.
///
/// The two `None` arms are exactly the fall-throughs: a resolvable
/// `allowedModes` slot that reads back as something other than an `Int`, and an
/// object with no such field whose synthetic slot is likewise not an `Int`.
/// Every other path returns a value the VM actually read.
pub(crate) fn lk_read_allowed_modes_opt(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if let Some(slot) = lk_allowed_modes_slot(ctx, this) {
        if let Value::Int(m) = ctx.get_field(this, slot) {
            return Some(m);
        }
        if let Value::Int(m) = ctx.get_field_by_name(this, "allowedModes") {
            return Some(m);
        }
        return None;
    }
    if let Value::Int(m) = ctx.get_field(this, LK_SYNTHETIC_ALLOWED_MODES) {
        return Some(m);
    }
    None
}

// ---------------------------------------------------------------------------
// Lookup access control (`allowedModes`)
// ---------------------------------------------------------------------------
//
// JDK 25 `java.lang.invoke.MethodHandles$Lookup` mode bits. Read off the
// class itself, not from memory:
//
//   PUBLIC=1 PRIVATE=2 PROTECTED=4 PACKAGE=8 MODULE=16 UNCONDITIONAL=32
//   ORIGINAL=64
//
// and the two factories we have to be exactly right about:
//
//   MethodHandles.lookup().lookupModes()       == 95  (0x5F, no UNCONDITIONAL)
//   MethodHandles.publicLookup().lookupModes() == 32  (0x20, UNCONDITIONAL ONLY)
//
// `publicLookup()` is *not* `PUBLIC` — that surprise is the whole reason a
// naive `modes & PUBLIC` test would have been wrong here.
pub(crate) const LK_MODE_PUBLIC: i32 = 0x01;
pub(crate) const LK_MODE_PRIVATE: i32 = 0x02;
pub(crate) const LK_MODE_PROTECTED: i32 = 0x04;
pub(crate) const LK_MODE_PACKAGE: i32 = 0x08;
pub(crate) const LK_MODE_MODULE: i32 = 0x10;
/// `Lookup.UNCONDITIONAL`. The ONLY mode `publicLookup()` carries, and the one
/// whose access rule is about the target CLASS rather than the member: an
/// UNCONDITIONAL lookup reaches public members of PUBLIC types in
/// unconditionally-exported packages, and nothing else.
pub(crate) const LK_MODE_UNCONDITIONAL: i32 = 0x20;
/// `FULL_POWER_MODES` = PUBLIC|PRIVATE|PROTECTED|PACKAGE|MODULE (no ORIGINAL,
/// no UNCONDITIONAL).
pub(crate) const LK_MODE_FULL_POWER: i32 =
    LK_MODE_PUBLIC | LK_MODE_PRIVATE | LK_MODE_PROTECTED | LK_MODE_PACKAGE | LK_MODE_MODULE;

/// Access flags of `member_name` as seen from the class `target_mirror`
/// denotes. Methods may be inherited, so walk the superclass chain for them;
/// fields resolve on the declaring class only.
///
/// `None` means "we could not answer" — the caller must then ALLOW, so a
/// class whose members we do not model (synthetic stub, native-registry-only
/// class) is never spuriously refused.
pub(crate) fn lk_member_access_flags(
    ctx: &dyn NativeContext,
    target_mirror: ObjectRef,
    member_name: &str,
    is_field: bool,
) -> Option<u16> {
    lk_member_access_flags_of(ctx, target_mirror, member_name, None, is_field)
}

/// [`lk_member_access_flags`], a method matched on its descriptor too when
/// `method_desc` is given (the requested `MethodType`); a descriptor no
/// declaration matches falls back to the first of that name, as before.
/// Interpreter round i1 wave 48, lane L4 (an overloaded constructor answered
/// with the first `<init>`'s modifiers).
pub(crate) fn lk_member_access_flags_of(
    ctx: &dyn NativeContext,
    target_mirror: ObjectRef,
    member_name: &str,
    method_desc: Option<&str>,
    is_field: bool,
) -> Option<u16> {
    if let (Some(desc), false) = (method_desc, is_field) {
        let mut cid = mirror_class_id(ctx, target_mirror)?;
        for _ in 0..256 {
            if let Some(m) = ctx
                .declared_methods(cid)
                .into_iter()
                .find(|m| m.name == member_name && m.descriptor == desc)
            {
                return Some(m.access_flags);
            }
            if member_name == "<init>" {
                break;
            }
            match ctx.superclass_of(cid) {
                Some(parent) if parent != cid => cid = parent,
                _ => break,
            }
        }
    }
    let mut cid = mirror_class_id(ctx, target_mirror)?;
    loop {
        if is_field {
            return ctx
                .declared_fields(cid)
                .into_iter()
                .find(|f| f.name == member_name)
                .map(|f| f.access_flags);
        }
        if let Some(m) = ctx
            .declared_methods(cid)
            .into_iter()
            .find(|m| m.name == member_name)
        {
            return Some(m.access_flags);
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => return None,
        }
    }
}

/// Refuse a `Lookup.find*` resolution the Lookup's `allowedModes` does not
/// permit.
///
/// `args` is the raw native argument slice: `args[0]` is the receiving
/// `Lookup`, `args[1]` the `refc` Class mirror, `args[name_idx]` the member
/// name String (pass `name_idx == usize::MAX` together with `literal` for
/// `findConstructor`, whose member is always `<init>`).
///
/// ## Why this is narrow on purpose
///
/// An over-strict check here breaks every framework that legitimately uses
/// `MethodHandles.lookup()` — Spring, Hibernate, Jackson, Groovy, ByteBuddy
/// and the JDK's own `LambdaMetafactory` all reach their OWN private members
/// through it. So the rule is **mode bits only**:
///
/// * `allowedModes == 0` — a Lookup nobody populated (or one whose modes we
///   could not read). Allow, exactly as before this check existed.
/// * `allowedModes & PRIVATE != 0` — a full-power lookup
///   (`MethodHandles.lookup()` = 0x5F, `privateLookupIn` = 0x1F, the JDK's
///   TRUSTED lookup = -1). Allow unconditionally, and short-circuit BEFORE
///   the allocating `declared_methods` walk. This is the clause that keeps
///   in-class private access working, and it does not consult `lookupClass`
///   at all — deliberately, because our `lookupClass` comes from a stack walk
///   and a wrong answer there must never turn into a refusal.
/// * otherwise the member's own modifier decides which bit is required:
///   `private` needs PRIVATE, `protected` needs PRIVATE|PROTECTED|PACKAGE,
///   package-private needs PRIVATE|PACKAGE, and `public` needs nothing.
///
/// One rule is NOT about the member at all and is enforced separately:
/// `UNCONDITIONAL` (`publicLookup()`, 0x20) reaches public members of PUBLIC
/// types only, so the target CLASS's own accessibility decides. That half is
/// checked; the "unconditionally exported package" half is not, for want of a
/// module graph.
///
/// Everything else the JLS §6.6 / `Lookup` contract requires — nestmate
/// relationships, `protected`-receiver rules, module `exports`/`opens` — is NOT
/// enforced. That residual is one-directional: it can only admit something
/// HotSpot would refuse, never refuse something HotSpot admits.
///
/// ## The refusal's text (wave 48)
///
/// HotSpot's, from `MemberName.makeAccessException` (measured, HotSpot 25,
/// `tools/probes/interp/L4/L4W48LookupFinderAccessText.java`), with
/// `ref_kind` the finder's reference kind (`invokeVirtual`,
/// `invokeInterface`, `getField`, ...):
///
/// * a METHOD or CONSTRUCTOR through `publicLookup()` fails the VM's own
///   resolution (its lookup class is `Object`): `no such method: <member>` /
///   `no such constructor: <member>`, no `from` clause;
/// * otherwise `Lookup.checkAccess`: `member is private` / `member is private
///   to package` / `member is protected` + `: <member>, from <lookup>`, where
///   a resolved constructor, and a private instance method, show
///   `invokeSpecial`;
/// * a zero-mode Lookup: `symbolic reference class is not accessible: class
///   C, from <lookup>`.
///
/// `<lookup>` needs Java (`Module.toString()`), so the refusal is returned as
/// an [`LkFindRefusal`] and rendered by [`LkFindRefusal::into_error`] once
/// nothing else is read from `args`.
pub(crate) fn lk_enforce_find_access(
    ctx: &dyn NativeContext,
    args: &[Value],
    name_idx: usize,
    literal: Option<&str>,
    is_field: bool,
    ref_kind: &str,
) -> Result<(), LkFindRefusal> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(()),
    };
    // `None` is "could not read the modes" and stays permissive; `Some(0)` is
    // the JDK's zero-mode Lookup, which is refused EVERY member including a
    // public one. See `lk_read_allowed_modes_opt` for why the two must not be
    // the same value here.
    let modes = match lk_read_allowed_modes_opt(ctx, this) {
        None => return Ok(()),
        Some(m) => m,
    };
    if (modes & LK_MODE_PRIVATE) != 0 {
        return Ok(());
    }
    let target = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(()),
    };
    if modes == 0 {
        // Refuse before the member walk: a zero-mode Lookup's answer does not
        // depend on the member's modifiers, and the walk allocates.
        let owner = lk_refusal_class_name(ctx, target);
        return Err(LkFindRefusal {
            head: format!("symbolic reference class is not accessible: class {owner}"),
            from: Some(lk_from_of(ctx, args)),
        });
    }
    // UNCONDITIONAL's rule is about the TARGET CLASS, not the member.
    // `publicLookup()` reaches public members of PUBLIC types only, so a public
    // member of a package-private class is refused — accessibility is the
    // class's, not the member's. Measured on OpenJDK 25.0.3:
    // `publicLookup().findStatic(<package-private class>, <a public static>)`
    // raises `IllegalAccessException: symbolic reference class is not
    // accessible`, while the same call against a public class succeeds.
    //
    // The other half of the JDK's rule — that the package be UNCONDITIONALLY
    // EXPORTED — still is not enforced, because there is no module graph to ask.
    // That residual stays one-directional (it can only admit what HotSpot
    // refuses), and this half removes the case a program actually meets:
    // publicLookup() over an application class that is not public.
    if modes == LK_MODE_UNCONDITIONAL && !crate::lang_class::mirror_is_public(ctx, target) {
        let owner = lk_refusal_class_name(ctx, target);
        return Err(LkFindRefusal {
            head: format!(
                "symbolic reference class is not accessible: class {owner}, from public Lookup"
            ),
            from: None,
        });
    }
    let name: String = match literal {
        Some(n) => n.to_string(),
        None => match args.get(name_idx) {
            Some(Value::Object(Some(o))) => match ctx.read_string(*o) {
                Some(s) => s,
                None => return Ok(()),
            },
            _ => return Ok(()),
        },
    };
    // The requested type, the argument after the name (`args[2]` for
    // `findConstructor`, which has none): a `MethodType` for a method or
    // constructor, the field's `Class` for a field. A method's picks the
    // overload whose modifiers decide (wave 48: `findConstructor` read the
    // first `<init>`'s). Unreadable renders `?`.
    let type_idx = if literal.is_some() {
        2
    } else {
        name_idx.saturating_add(1)
    };
    let requested = match args.get(type_idx) {
        Some(Value::Object(Some(t))) if is_field => {
            Some(mirror_to_descriptor(ctx, *t).into_owned())
        }
        Some(Value::Object(Some(t))) => Some(descriptor_from_method_type(ctx, *t)),
        _ => None,
    };
    let method_desc = requested.as_deref().filter(|_| !is_field);
    let flags = match lk_member_access_flags_of(ctx, target, &name, method_desc, is_field) {
        Some(f) => f,
        None => return Ok(()),
    };
    let required = lk_modes_required_for_member(i32::from(flags));
    if required == 0 || (modes & required) != 0 {
        return Ok(());
    }
    let owner = mirror_class_name(ctx, target).unwrap_or_else(|| "?".to_string());
    let ty = requested.unwrap_or_else(|| {
        if is_field {
            "?".to_string()
        } else {
            "()?".to_string()
        }
    });
    let ctor = !is_field && name == "<init>";
    if modes == LK_MODE_UNCONDITIONAL && !is_field {
        let what = if ctor {
            "no such constructor"
        } else {
            "no such method"
        };
        return Err(LkFindRefusal {
            head: format!(
                "{what}: {}",
                lk_method_member_display(&owner, &name, &ty, ref_kind)
            ),
            from: None,
        });
    }
    let private = flags & cratonvm_types::access_flags::ACC_PRIVATE != 0;
    let what = if private {
        "member is private"
    } else if flags & cratonvm_types::access_flags::ACC_PROTECTED != 0 {
        "member is protected"
    } else {
        "member is private to package"
    };
    let member = if is_field {
        lk_field_member_display(&owner, &name, &ty, ref_kind)
    } else {
        // A resolved constructor, and a resolved private instance method, are
        // `invokeSpecial` members (measured: rows `pkg-private-ctor`,
        // `pkg-private-virtual`).
        // A final method found virtually is one too (interpreter round i5
        // wave 108, lane L3: HotSpot's statically bound `direct_call`, as
        // `native_mhn_resolve` now settles it).
        let final_virtual = ref_kind == "invokeVirtual"
            && flags & cratonvm_types::access_flags::ACC_FINAL != 0;
        let shown_kind =
            if ctor
                || final_virtual
                || (private && matches!(ref_kind, "invokeVirtual" | "invokeInterface"))
            {
                "invokeSpecial"
            } else {
                ref_kind
            };
        lk_method_member_display(&owner, &name, &ty, shown_kind)
    };
    Err(LkFindRefusal {
        head: format!("{what}: {member}"),
        from: Some(lk_from_of(ctx, args)),
    })
}

/// A `Lookup.find*` access refusal decided by [`lk_enforce_find_access`]:
/// the message up to its `, from <lookup>` clause, and whose clause it is
/// (`None`: the head is the whole message). Interpreter round i1 wave 48,
/// lane L4.
#[derive(Debug)]
pub(crate) struct LkFindRefusal {
    pub(crate) head: String,
    pub(crate) from: Option<LkFrom>,
}

impl LkFindRefusal {
    /// The `IllegalAccessException` (a REAL, checked one, which is what
    /// `Lookup.find*` declares and what callers catch). Renders the `from`
    /// clause, which runs Java: call it after the last read of `args`.
    pub(crate) fn into_error(self, ctx: &mut dyn NativeContext) -> MethodCallFailed {
        let suffix = match self.from {
            Some(from) => lk_from_suffix(ctx, from),
            None => String::new(),
        };
        RuntimeError::IllegalAccessException {
            message: format!("{}{suffix}", self.head),
        }
        .into()
    }
}

/// The member-resolution access check HotSpot makes for a `Lookup` METHOD or
/// CONSTRUCTOR finder (interpreter round i3 wave 81, lane L4;
/// `CRATONVM_MH_COMPAT_FINDER_NEST_ACCESS`, default on, kill switch `=0`).
///
/// [`lk_enforce_find_access`] stops at the lookup's `PRIVATE` bit on purpose
/// (it does not trust `lookupClass`). HotSpot's `MHN_resolve_Mem` does not
/// stop there: it resolves the member with the lookup class as the accessing
/// class, and a PRIVATE member of a class that is neither the lookup class
/// nor its nestmate fails with `IllegalAccessError`, which
/// `MemberName.makeAccessException` turns into `IllegalAccessException: no
/// such method: <member>` (`no such constructor` for `<init>`, no `from`
/// clause). `--jdk-only` makes the same check in [`native_mhn_resolve`]
/// (`caller_may_access_member`) with the lookup class the same stack walk
/// answers, so `--compatible` trusts it as much. Measured by
/// `tools/probes/interp/L4/L4W81MethodResolveMemo.java` rows `priv-other`,
/// `special-other`, `ctor-other` (`--compatible` answered a handle).
///
/// Only a PRIVATE member is judged (package and protected access through a
/// `PRIVATE`-mode lookup stay admitted, as before); a TRUSTED lookup (`-1`),
/// a lookup without `PRIVATE` (the caller above decides), an unreadable
/// lookup class, target, name or type, and a member not found all admit.
/// Reads only: no allocation, no Java, so `args` stays current.
pub(crate) fn lk_enforce_private_nest_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name_idx: usize,
    literal: Option<&str>,
    ref_kind: &str,
) -> Result<Option<Vec<Value>>, MethodCallFailed> {
    // Interpreter round i4 wave 92, lane L4: a PACKAGE-PRIVATE method of
    // another run-time package fails HotSpot's resolution the same way
    // (`LinkResolver::check_method_accessability`), under
    // `CRATONVM_MH_COMPAT_FINDER_LOOKUP_CLASS_ACCESS` (default on, `=0` off).
    let private_rule = crate::nbflags().mh_compat_finder_nest_access;
    let package_rule = crate::nbflags().mh_compat_finder_lookup_class_access;
    if !private_rule && !package_rule {
        return Ok(None);
    }
    let Some(Value::Object(Some(this))) = args.first() else {
        return Ok(None);
    };
    let Some(modes) = lk_read_allowed_modes_opt(&*ctx, *this) else {
        return Ok(None);
    };
    if modes == -1 || modes & LK_MODE_PRIVATE == 0 {
        return Ok(None);
    }
    let lookup_cid = match ctx.get_field(*this, 0) {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    };
    let Some(lookup_cid) = lookup_cid else {
        return Ok(None);
    };
    let Some(Value::Object(Some(target))) = args.get(1) else {
        return Ok(None);
    };
    let target = *target;
    if lk_lambda_proxy_of_mirror(&*ctx, target).is_some() {
        return Ok(None);
    }
    let Some(target_cid) = ctx.class_id_from_mirror(target) else {
        return Ok(None);
    };
    let name = match literal {
        Some(n) => n.to_string(),
        None => match args.get(name_idx) {
            Some(Value::Object(Some(s))) => match ctx.read_string(*s) {
                Some(n) => n,
                None => return Ok(None),
            },
            _ => return Ok(None),
        },
    };
    let type_idx = if literal.is_some() {
        2
    } else {
        name_idx.saturating_add(1)
    };
    let desc = match args.get(type_idx) {
        Some(Value::Object(Some(t))) => descriptor_from_method_type(&*ctx, *t),
        _ => return Ok(None),
    };
    if desc.is_empty() {
        return Ok(None);
    }
    // Method resolution: the class, then its superclasses (a constructor is
    // never inherited).
    let mut cid = target_cid;
    let mut found: Option<(ClassId, u16)> = None;
    for _ in 0..64 {
        if let Some(m) = ctx
            .declared_methods(cid)
            .iter()
            .find(|m| m.name == name && m.descriptor == desc)
        {
            found = Some((cid, m.access_flags));
            break;
        }
        if name == "<init>" {
            break;
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => break,
        }
    }
    let Some((declaring, flags)) = found else {
        return Ok(None);
    };
    let private = flags & cratonvm_types::access_flags::ACC_PRIVATE != 0;
    let judged = if private {
        private_rule
    } else if flags
        & (cratonvm_types::access_flags::ACC_PUBLIC | cratonvm_types::access_flags::ACC_PROTECTED)
        == 0
    {
        // Package-private. Only when `refc` itself is accessible: otherwise
        // the JDK's `checkSymbolicClass` refuses first, with its own text.
        package_rule && lk_refc_accessible(ctx, lookup_cid, target, target_cid)
    } else {
        // Public, or protected (whose resolution rule also asks the
        // resolved class; left admitted, a one-directional residual).
        false
    };
    if !judged || lookup_cid == declaring {
        return Ok(None);
    }
    // The nest hosts are loaded before a PRIVATE member is judged, as
    // HotSpot's `MHN_resolve_Mem` -> `InstanceKlass::nest_host` does (JIT
    // round 20 wave 16, lane nest; `lang_class::settle_reflective_nest_hosts`).
    // A GC point only when a host is loaded: every argument is pinned there
    // and handed back refreshed, and the finder reads its arguments from that
    // copy.
    let mut target = target;
    let refreshed = if private {
        crate::lang_class::settle_reflective_nest_hosts(
            ctx,
            lookup_cid,
            declaring,
            i32::from(flags),
            &mut [&mut target],
            &mut [],
            args,
        )
    } else {
        None
    };
    if crate::lang_reflect::caller_may_access_member(
        ctx,
        lookup_cid,
        declaring,
        i32::from(flags),
        None,
    ) {
        return Ok(refreshed);
    }
    let owner = mirror_class_name(ctx, target).unwrap_or_else(|| "?".to_string());
    let what = if name == "<init>" {
        "no such constructor"
    } else {
        "no such method"
    };
    Err(lookup_access_error(format!(
        "{what}: {}",
        lk_method_member_display(&owner, &name, &desc, ref_kind)
    )))
}

/// May `lookup_cid` name `refc` (mirror `refc_mirror`) at all? HotSpot's
/// `VerifyAccess.isClassAccessible` for a full-power lookup: a public class,
/// or one of the lookup class's own run-time package. The module half is not
/// asked (no module graph), so `true` can over-admit, never over-refuse.
pub(crate) fn lk_refc_accessible(
    ctx: &mut dyn NativeContext,
    lookup_cid: ClassId,
    refc_mirror: ObjectRef,
    refc: ClassId,
) -> bool {
    crate::lang_class::mirror_is_public(&*ctx, refc_mirror)
        // Modifiers `0` (package access): the lookup class itself, or a class
        // of its run-time package.
        || crate::lang_reflect::caller_may_access_member(ctx, lookup_cid, refc, 0, None)
}

/// The FIELD finders' half of the lookup-class question (interpreter round i4
/// wave 92, lane L4; proposal `i40-L4-proposal-lookup-finders-ask-the-lookup-class`;
/// `CRATONVM_MH_COMPAT_FINDER_LOOKUP_CLASS_ACCESS`, default on, kill switch
/// `=0`).
///
/// [`lk_enforce_find_access`] admits every member for a `Lookup` holding
/// `PRIVATE` without looking at `lookupClass`. HotSpot's field finders
/// resolve the field WITHOUT an access check (`MethodHandles::resolve_MemberName`'s
/// `IS_FIELD` arm), then `Lookup.checkField` -> `checkAccess` asks
/// `VerifyAccess.isMemberAccessible(refc, defc, mods, lookupClass, ...)`:
///
/// * `defc == lookupClass`: admitted;
/// * private: `Reflection.areNestMates(defc, lookupClass)`;
/// * package-private: the same run-time package;
/// * protected: the same run-time package, or `lookupClass` a subclass of
///   `defc` (the `protected static` + `isRelatedClass(refc, lookupClass)`
///   narrowing is not modelled: a one-directional residual);
///
/// and refuses with `IllegalAccessException: member is private|private to
/// package|protected: Defc.name/Type/refKind, from <lookup>` (the texts of
/// [`lk_enforce_find_access`], wave 48). Under `--jdk-only` the finders run
/// the JDK's own `Lookup` bytecode, which asks the same question.
///
/// Admits (leaves the decision to what follows) whatever HotSpot answers
/// BEFORE the access check, or with another text: a TRUSTED lookup (`-1`), a
/// lookup without `PRIVATE` (the mode-bit gate decides), an unreadable lookup
/// class, `refc`, name or type, a field not found by name AND type
/// (`NoSuchFieldException`), a static-ness mismatch (`expected a (non-)static
/// field`), a setter of a final field (`unexpected set of a final field`), and
/// a `refc` the lookup class cannot name (`symbolic reference class is not
/// accessible`). The resolution walk is `refc` then its superclasses; a field
/// a superINTERFACE declares is public static, so it never refuses.
pub(crate) fn lk_enforce_lookup_class_field_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ref_kind: &str,
) -> Result<(), MethodCallFailed> {
    use cratonvm_types::access_flags::{ACC_FINAL, ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC};
    if !crate::nbflags().mh_compat_finder_lookup_class_access {
        return Ok(());
    }
    let Some(Value::Object(Some(this))) = args.first() else {
        return Ok(());
    };
    let Some(modes) = lk_read_allowed_modes_opt(&*ctx, *this) else {
        return Ok(());
    };
    if modes == -1 || modes & LK_MODE_PRIVATE == 0 {
        return Ok(());
    }
    let lookup_cid = match ctx.get_field(*this, 0) {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    };
    let Some(lookup_cid) = lookup_cid else {
        return Ok(());
    };
    let Some(Value::Object(Some(target))) = args.get(1) else {
        return Ok(());
    };
    let target = *target;
    if lk_lambda_proxy_of_mirror(&*ctx, target).is_some() {
        return Ok(());
    }
    let Some(refc) = ctx.class_id_from_mirror(target) else {
        return Ok(());
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(s))) => match ctx.read_string(*s) {
            Some(n) => n,
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    let field_desc = match args.get(3) {
        Some(Value::Object(Some(t))) => mirror_to_descriptor(&*ctx, *t).into_owned(),
        _ => return Ok(()),
    };
    // Field resolution by name AND type: the class, then its superclasses.
    let mut cid = refc;
    let mut found: Option<(ClassId, u16, bool)> = None;
    for _ in 0..256 {
        if let Some(f) = ctx
            .declared_fields(cid)
            .iter()
            .find(|f| f.name == name && f.descriptor == field_desc)
        {
            found = Some((cid, f.access_flags, f.is_static));
            break;
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => break,
        }
    }
    let Some((declaring, flags, is_static)) = found else {
        return Ok(());
    };
    let want_static = matches!(ref_kind, "getStatic" | "putStatic");
    let setter = matches!(ref_kind, "putField" | "putStatic");
    if is_static != want_static
        || (setter && flags & ACC_FINAL != 0)
        || flags & ACC_PUBLIC != 0
        || declaring == lookup_cid
    {
        return Ok(());
    }
    if !lk_refc_accessible(ctx, lookup_cid, target, refc)
        || crate::lang_reflect::caller_may_access_member(
            ctx,
            lookup_cid,
            declaring,
            i32::from(flags),
            None,
        )
    {
        return Ok(());
    }
    let what = if flags & ACC_PRIVATE != 0 {
        "member is private"
    } else if flags & ACC_PROTECTED != 0 {
        "member is protected"
    } else {
        "member is private to package"
    };
    let owner = ctx
        .class_name_of_id(declaring)
        .unwrap_or_else(|| "?".to_string());
    let refusal = LkFindRefusal {
        head: format!(
            "{what}: {}",
            lk_field_member_display(&owner, &name, &field_desc, ref_kind)
        ),
        from: Some(lk_from_of(&*ctx, args)),
    };
    Err(refusal.into_error(ctx))
}

/// `Lookup.findVirtual`'s reference kind: `invokeInterface` when `refc`
/// (`args[1]`) is an interface. Reads no heap object but the mirror.
pub(crate) fn lk_virtual_ref_kind(ctx: &dyn NativeContext, args: &[Value]) -> &'static str {
    match args.get(1) {
        Some(Value::Object(Some(m)))
            if ctx
                .class_id_from_mirror(*m)
                .is_some_and(|cid| ctx.is_interface_class(cid)) =>
        {
            "invokeInterface"
        }
        _ => "invokeVirtual",
    }
}

/// Which `allowedModes` bits admit a member whose access flags are `flags`?
/// **Any one** of the returned bits is enough. `0` means "no mode bit is
/// required" — the member is `public`, and only the target class's own
/// accessibility can still refuse it.
///
/// Shared by [`lk_enforce_find_access`] and [`lk_enforce_unreflect_access`] on
/// purpose. `Lookup.findVirtual(C, "m", t)` and
/// `Lookup.unreflect(C.getDeclaredMethod("m"))` are the same access question
/// asked two ways — the JDK routes both into the same `getDirectMethod`
/// (`MethodHandles.java`) — so the two gates must not be able to drift apart.
/// Two predicates answering one question differently is the shape that produced
/// several defects in this campaign.
pub(crate) fn lk_modes_required_for_member(flags: i32) -> i32 {
    use cratonvm_types::access_flags::{ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC};
    if (flags & i32::from(ACC_PUBLIC)) != 0 {
        return 0;
    }
    if (flags & i32::from(ACC_PRIVATE)) != 0 {
        LK_MODE_PRIVATE
    } else if (flags & i32::from(ACC_PROTECTED)) != 0 {
        LK_MODE_PRIVATE | LK_MODE_PROTECTED | LK_MODE_PACKAGE
    } else {
        LK_MODE_PRIVATE | LK_MODE_PACKAGE
    }
}

/// How an `unreflect*` entry point treats the reflective object's
/// `setAccessible` flag. The three arms are not a style choice — the JDK 25
/// javadoc states a different rule for each, and the two that differ from the
/// common case are stated explicitly *because* they differ.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LkUnreflectKind {
    /// `unreflect(Method)`, `unreflectConstructor(Constructor)`,
    /// `unreflectGetter(Field)`, `unreflectSetter(Field)`.
    ///
    /// > "If the method's `accessible` flag is not set, access checking is
    /// > performed immediately on behalf of the lookup class."
    ///
    /// — and the implementation is the literal reading of that sentence:
    /// `Lookup lookup = m.isAccessible() ? IMPL_LOOKUP : this;`. `IMPL_LOOKUP`
    /// is the TRUSTED lookup, so a set flag does not soften the check, it
    /// replaces the Lookup that performs it. Nothing is refused.
    AccessibleWaives,
    /// `unreflectVarHandle(Field)`.
    ///
    /// > "Access checking is performed immediately on behalf of the lookup
    /// > class, **regardless of the value of the field's `accessible` flag**."
    ///
    /// The JDK body reads `f.isAccessible()` nowhere, unlike `unreflectField`
    /// three methods above it.
    AccessibleIgnored,
    /// `unreflectSpecial(Method, Class)`.
    ///
    /// > "Before method resolution, if the explicitly specified caller class is
    /// > not identical with the lookup class, or if this lookup object does not
    /// > have private access privileges, the access fails."
    ///
    /// `checkSpecialCaller` runs before anything else and the body carries the
    /// comment `// ignore m.isAccessible:  this is a new kind of access`. Only
    /// the private-access half is enforced here — see the note in
    /// [`lk_enforce_unreflect_access`] on why the `specialCaller` half is not.
    Special,
}

/// Refuse an `unreflect*` conversion the Lookup's `allowedModes` does not
/// permit — the reflection-shaped twin of [`lk_enforce_find_access`], and the
/// gate the `unreflect` family never had.
///
/// ## Why this is a real hole and not a theoretical one
///
/// `Lookup.find*` has consulted `allowedModes` since the W4-1 fix, so
/// `publicLookup().findVirtual(Holder.class, "secret", …)` is refused. The
/// `unreflect` family reached the identical member with the identical Lookup
/// and was admitted, because it never asked: one line of Java
/// (`pub.unreflect(Holder.class.getDeclaredMethod("secret", int.class))`)
/// walked around the whole check. The two entry points must answer alike; the
/// JDK funnels them into the same `getDirectMethod`/`getDirectField`, which is
/// where its own check lives.
///
/// ## The rule, and what it deliberately does not ask
///
/// **Mode bits only**, exactly as [`lk_enforce_find_access`], and sharing
/// [`lk_modes_required_for_member`] so the two cannot drift:
///
/// * modes unreadable (`None`) -> allow. The valve. A Lookup shape this VM does
///   not model must never become an `IllegalAccessException`.
/// * `PRIVATE` set -> allow, before touching the member. `MethodHandles.lookup()`
///   (0x5F), `privateLookupIn` (0x1F) and the JDK's TRUSTED lookup (-1) all land
///   here, which is every Lookup Spring / Hibernate / Jackson / Groovy /
///   ByteBuddy / `LambdaMetafactory` ever hold. They cannot be refused by this
///   check at all.
/// * the reflective object's `accessible` flag, for the arms whose javadoc says
///   it waives the check -> allow.
/// * `modes == 0` -> refuse everything, public members included (measured in
///   [`lk_read_allowed_modes_opt`]: `lookup().dropLookupMode(PUBLIC)` is 0 and
///   refuses a public method of a public class).
/// * `UNCONDITIONAL` alone (`publicLookup()`, 0x20) -> the target CLASS must be
///   public. Same rule, same measurement, as the find* gate.
/// * otherwise the member's own modifier picks the required bit.
/// * member modifiers unreadable -> allow.
///
/// **`lookupClass` is not consulted, here or in the find* gate.** Ours comes
/// from a stack walk in the `MethodHandles.lookup()` native; a wrong answer
/// there must never turn into a refusal. That is what keeps the whole nestmate
/// / same-package / `protected`-receiver family out of this function, and it is
/// also why `unreflectSpecial`'s `specialCaller != lookupClass()` conjunct is
/// NOT enforced — only its `(lookupModes() & PRIVATE) == 0` half is. Both
/// omissions are one-directional: they can admit something HotSpot refuses,
/// never refuse something HotSpot admits.
///
/// **Mode scope: every mode.** This is JDK reflection semantics, not a
/// strictness policy, so it is not gated on `--jdk-only` — the same reasoning
/// under which the `find*` gate, the `setAccessible` gate and the `exports`
/// gate all run unconditionally. In `Compatible` (`--real-jdk`) mode the only
/// behaviour it can change is a case CratonVM answered differently from HotSpot
/// 25, and the PRIVATE short-circuit above means no framework Lookup reaches
/// the refusal. It cannot fire at all in `--synthetic-jdk`: there,
/// `register_classloader_natives` runs LAST and its `lk_unreflect` /
/// `lk_unreflect_special` win the registration, so these natives are not even
/// the live ones (see the ordering note in
/// `classloader.rs::register_classloader_natives`); the four this file alone
/// registers still route here, and answer from the same mode bits.
///
/// Called as the FIRST statement of each native for the same reason
/// [`lk_enforce_find_access`] is: `args` still holds the ObjectRefs the VM
/// handed over and nothing has had a chance to allocate and move them. Every
/// read below is a field/flags read; none allocates on the Java heap.
pub(crate) fn lk_enforce_unreflect_access(
    ctx: &dyn NativeContext,
    args: &[Value],
    kind: LkUnreflectKind,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(()),
    };
    // `None` is "could not read the modes" and stays permissive; `Some(0)` is
    // the JDK's zero-mode Lookup, which refuses every member.
    let modes = match lk_read_allowed_modes_opt(ctx, this) {
        None => return Ok(()),
        Some(m) => m,
    };
    // The overwhelmingly common answer, and the cheapest: short-circuit before
    // reading anything off the reflective object.
    if (modes & LK_MODE_PRIVATE) != 0 {
        return Ok(());
    }
    let member = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // A null/absent member is the native's own error to raise (NPE on
        // HotSpot); it is not an access decision.
        _ => return Ok(()),
    };

    if kind == LkUnreflectKind::Special {
        // `checkSpecialCaller`: `if (allowedModes == TRUSTED) return;` —
        // covered by the PRIVATE short-circuit above, since TRUSTED is -1 —
        // `if ((lookupModes() & PRIVATE) == 0 || …) throw`. Reaching here means
        // the PRIVATE bit is clear, so the first disjunct has already decided.
        let owner = match ctx.get_field_by_name(member, "clazz") {
            Value::Object(Some(m)) => mirror_class_name(ctx, m).unwrap_or_default(),
            _ => String::new(),
        };
        return Err(
            cratonvm_types::error::RuntimeError::IllegalAccessException {
                message: format!(
                    "no private access for invokespecial: class {}, from Lookup with \
                 modes 0x{modes:04x}",
                    owner.class_text(&*ctx)
                ),
            }
            .into(),
        );
    }

    // JDK: `Lookup lookup = m.isAccessible() ? IMPL_LOOKUP : this;`. The flag
    // does not weaken the check, it hands it to the TRUSTED lookup — so a set
    // flag is an unconditional allow, and reading it is only worth doing on the
    // arms whose javadoc says so.
    if kind == LkUnreflectKind::AccessibleWaives
        && crate::lang_class::accessible_override_is_set(ctx, member)
    {
        return Ok(());
    }

    let declaring = match ctx.get_field_by_name(member, "clazz") {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let owner = || {
        declaring
            .and_then(|m| mirror_class_name(ctx, m))
            .unwrap_or_else(|| "?".to_string())
            .class_text(&*ctx)
    };

    if modes == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalAccessException {
                message: format!(
                    "no access: {} from Lookup with modes 0x0000 (no lookup modes remain)",
                    owner()
                ),
            }
            .into(),
        );
    }
    // UNCONDITIONAL's rule is about the TARGET CLASS, not the member:
    // `publicLookup()` reaches public members of PUBLIC types only. Identical
    // to the find* gate's arm, including its measurement.
    if modes == LK_MODE_UNCONDITIONAL
        && declaring.is_some_and(|m| !crate::lang_class::mirror_is_public(ctx, m))
    {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalAccessException {
                message: format!(
                    "symbolic reference class is not accessible: class {}, from public Lookup",
                    owner()
                ),
            }
            .into(),
        );
    }

    // The member's own flags are already on the reflective object — no
    // `declared_methods` walk, unlike the find* gate which only has a name.
    // An unreadable `modifiers` slot is the same valve as an unresolvable
    // member there: allow.
    let Value::Int(flags) = ctx.get_field_by_name(member, "modifiers") else {
        return Ok(());
    };
    let required = lk_modes_required_for_member(flags);
    if required == 0 || (modes & required) != 0 {
        return Ok(());
    }
    let member_name = match ctx.get_field_by_name(member, "name") {
        // A `Constructor` has no `name` field; `get_field_by_name` answers
        // `Object(None)` and the JDK's own name for the member is `<init>`.
        Value::Object(Some(n)) => ctx.read_string(n).unwrap_or_else(|| "<init>".to_string()),
        _ => "<init>".to_string(),
    };
    // A REAL `java.lang.IllegalAccessException` (checked) — what every
    // `unreflect*` overload declares, and what callers catch.
    Err(
        cratonvm_types::error::RuntimeError::IllegalAccessException {
            message: format!(
                "no access: {}.{member_name} (modifiers 0x{flags:04x}) \
             from Lookup with modes 0x{modes:04x}",
                owner()
            ),
        }
        .into(),
    )
}

/// Do the two `Class` mirrors live in the same package?
///
/// A mirror we cannot name answers `true` — "cannot tell, so do not narrow".
///
/// **Superseded and CALLER-FREE.** `Lookup.in` was its only caller and now uses
/// `classloader::lk_class_relation`, which answers the same question plus the
/// two this one cannot: is the target the lookup class itself, and is it a
/// member of the same top-level class (`VerifyAccess.isSamePackageMember`).
/// A same-package test ALONE cannot see the nestmate reduction, so anything
/// wired back to this function will over-grant PRIVATE|PROTECTED — measured 31
/// where OpenJDK 25.0.3 returns 25. Use `lk_class_relation` instead; this is
/// kept only because two docs refer to it by name.
pub(crate) fn lk_same_package(ctx: &dyn NativeContext, a: Value, b: Value) -> bool {
    fn package_of(ctx: &dyn NativeContext, v: Value) -> Option<String> {
        let m = match v {
            Value::Object(Some(m)) => m,
            _ => return None,
        };
        let name = mirror_class_name(ctx, m)?;
        Some(match name.rfind('/') {
            Some(i) => name[..i].to_string(),
            None => String::new(),
        })
    }
    match (package_of(ctx, a), package_of(ctx, b)) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// `PRIVATE|MODULE` — the exact pair `MethodHandles.privateLookupIn` demands
/// of its `caller` argument, and the pair whose absence produces the JDK's
/// `"caller does not have PRIVATE and MODULE lookup mode"`.
pub(crate) const LK_MODE_PRIVATE_AND_MODULE: i32 = LK_MODE_PRIVATE | LK_MODE_MODULE;

/// The refusals `MethodHandles.privateLookupIn(targetClass, caller)` performs
/// before it mints anything.
///
/// This native used to grant `0x1F` **unconditionally** — it never looked at
/// `caller` at all — so every caller the JDK refuses got full private access
/// to the target instead. Re-measured for this lane on OpenJDK 25.0.3
/// (`p.PliProbe`, caller class `p.PliProbe` in the unnamed module):
///
/// ```text
/// caller                       target                     answer
/// ---------------------------- -------------------------- ---------------------------------------------
/// lookup()            (95)     p.Mate                     OK modes=31 prev=null
/// lookup()            (95)     p.PliProbe (own class)     OK modes=31 prev=null
/// lookup()            (95)     p.PliProbe$Nested          OK modes=31 prev=null
/// lookup()            (95)     q.Other (other package)    OK modes=31 prev=null
/// dropLookupMode(PROTECTED)(27) p.Mate                    OK modes=31 prev=null   (27 still has PRIVATE|MODULE)
/// dropLookupMode(PRIVATE) (25) p.Mate                     IllegalAccessException: caller does not have PRIVATE and MODULE lookup mode
/// dropLookupMode(MODULE)   (1) p.Mate                     IllegalAccessException: (same)
/// dropLookupMode(PACKAGE) (17) p.Mate                     IllegalAccessException: (same)
/// dropLookupMode(PUBLIC)   (0) p.Mate                     IllegalAccessException: (same)
/// publicLookup()          (32) p.Mate                     IllegalAccessException: (same)
/// lookup().in(q.Other)    (17) q.Other                    IllegalAccessException: (same)
/// lookup().in(String)      (1) p.Mate                     IllegalAccessException: (same)
/// lookup()            (95)     java.lang.String           IllegalAccessException: module java.base does not open java.lang to unnamed module @8bcc55f
/// lookup()            (95)     java.util.HashMap          IllegalAccessException: module java.base does not open java.util to unnamed module @8bcc55f
/// lookup()            (95)     int.class / void.class     IllegalArgumentException: int is a primitive class
/// lookup()            (95)     int[].class                IllegalArgumentException: class [I is an array class
/// lookup()            (95)     p.Mate[].class             IllegalArgumentException: class [Lp.Mate; is an array class
/// lookup()            (95)     null                       NullPointerException: Cannot invoke "java.lang.Class.isPrimitive()" because "targetClass" is null
/// null                         p.Mate                     NullPointerException: Cannot read field "allowedModes" because "caller" is null
/// lookup()            (95)     java.lang.String           OK modes=15 prev=p.PliProbe2   *with* --add-opens java.base/java.lang=ALL-UNNAMED
/// ```
///
/// Two orderings fall straight out of the null cases and are reproduced here:
/// `caller.allowedModes` is read FIRST (a null `caller` NPEs before the target
/// is examined at all), and the primitive/array `IllegalArgumentException`s
/// come BEFORE the mode check — measured, `publicLookup()` with `int.class`
/// raises the primitive `IllegalArgumentException`, not the access one.
///
/// ## What is enforced, and the one deliberate hole
///
/// * `caller == null` / `targetClass == null` — the two NPEs above.
/// * `allowedModes == -1` (`TRUSTED`) — the JDK returns `new Lookup(targetClass)`
///   from the top of the method without running ANY of the checks below, so we
///   short-circuit in the same place.
/// * primitive / array target — `IllegalArgumentException`, JDK wording.
/// * `(modes & PRIVATE|MODULE) != PRIVATE|MODULE` — `IllegalAccessException`,
///   **except** when the mode word could not be read at all.
///
/// The valve is "could not read", not "reads zero", and the two used to be the
/// same value: [`lk_read_allowed_modes`] answered 0 both for a Lookup that
/// genuinely has no modes AND for one whose layout the VM does not model.
/// Refusing the "cannot tell" 0 would turn every unmodelled Lookup into an
/// `IllegalAccessException`, which is the one failure mode a new exception path
/// on this method must not have; admitting the genuine 0 let
/// `lookup().dropLookupMode(PUBLIC)` through, which HotSpot refuses.
/// [`lk_read_allowed_modes_opt`] separates them, so the refusal now fires on a
/// POSITIVELY read weak mode word — `publicLookup()` (0x20), an explicit
/// `dropLookupMode` (including the 0 case), and a narrowing `Lookup.in`
/// (0/1/17/25). All of them are cases HotSpot refuses too.
///
/// **Not enforced here**: the module `canRead`/`isOpen` pair, and the
/// cross-module success shape (`modes = 15`, `prevLookupClass = caller
/// class`) it gates. Both are answered after this gate by
/// `classloader::private_lookup_in_module_refusal` (interpreter round i1
/// waves 45-46 for `--jdk-only`, wave 47 for `--compatible`).
pub(crate) fn pli_enforce(
    ctx: &mut dyn NativeContext,
    target: Value,
    caller: Value,
) -> Result<(), MethodCallFailed> {
    // (1) `caller.allowedModes` — the JDK's first dereference.
    let caller_ref = match caller {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot read field \"allowedModes\" because \"caller\" is null".to_string(),
                ),
            }
            .into());
        }
    };
    // `None` (unreadable) keeps the permissive valve; `Some(0)` is a real
    // zero-mode Lookup and is refused. See `lk_read_allowed_modes_opt`.
    let read_modes = lk_read_allowed_modes_opt(ctx, caller_ref);
    let modes = read_modes.unwrap_or(0);
    // (2) TRUSTED short-circuits the whole method, primitive/array included.
    if modes == -1 {
        return Ok(());
    }
    // (3) `targetClass.isPrimitive()`.
    let target_ref = match target {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.lang.Class.isPrimitive()\" because \"targetClass\" is null"
                        .to_string(),
                ),
            }
            .into());
        }
    };
    // Read the name before the `&mut` borrow below, and keep it: it is both the
    // array discriminator and the text of both `IllegalArgumentException`s.
    let target_name = mirror_class_name(ctx, target_ref);
    let is_primitive = matches!(
        crate::lang_class::native_class_is_primitive(ctx, &[Value::Object(Some(target_ref))]),
        Ok(Some(Value::Int(1)))
    );
    if is_primitive {
        // `Class.toString()` of a primitive is bare — "int", "void" — so the
        // JDK's `targetClass + " is a primitive class"` has no "class " prefix.
        let name = target_name.unwrap_or_else(|| "?".to_string());
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("{name} is a primitive class"),
        }
        .into());
    }
    // (4) `targetClass.isArray()`. Array mirrors are the ones whose name starts
    // with '[' (`lang_class::native_class_is_array` uses the same test).
    // `Class.toString()` of an array IS prefixed, and prints the BINARY name:
    // "class [I", "class [Lp.Mate;".
    if let Some(name) = target_name {
        if name.starts_with('[') {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("class {} is an array class", name.class_text(&*ctx)),
            }
            .into());
        }
    }
    // (5) the mode gate, with the `modes == 0` valve documented above.
    if read_modes.is_some() && (modes & LK_MODE_PRIVATE_AND_MODULE) != LK_MODE_PRIVATE_AND_MODULE {
        return Err(RuntimeError::IllegalAccessException {
            message: "caller does not have PRIVATE and MODULE lookup mode".to_string(),
        }
        .into());
    }
    Ok(())
}

/// JDK 25 `Lookup.checkUnprivilegedlookupClass`, which `newLookup` runs for
/// every lookup `in` and `privateLookupIn` make (not for a TRUSTED caller's):
/// a lookup class of `java.lang.invoke` is `IllegalArgumentException:
/// illegal lookupClass: <Class.toString()>`. Measured on HotSpot 25:
/// `lookup().in(MethodHandle.class)` and `publicLookup().in(MethodHandle.class)`
/// throw `illegal lookupClass: class java.lang.invoke.MethodHandle`
/// (`tools/probes/interp/L4/L4W46LookupInJavaLangInvoke.java`). Interpreter
/// round i1 wave 46, lane L4.
pub(crate) fn lk_check_unprivileged_lookup_class(
    ctx: &mut dyn NativeContext,
    target: Value,
) -> Result<(), MethodCallFailed> {
    let Value::Object(Some(mirror)) = target else {
        return Ok(());
    };
    let Some(name) = mirror_class_name(ctx, mirror) else {
        return Ok(());
    };
    if !name.starts_with("java/lang/invoke/") {
        return Ok(());
    }
    let kind = match mirror_class_id(ctx, mirror) {
        Some(cid) if ctx.is_interface_class(cid) => "interface",
        _ => "class",
    };
    Err(RuntimeError::IllegalArgumentException {
        message: format!("illegal lookupClass: {kind} {}", name.class_text(&*ctx)),
    }
    .into())
}

/// The `Lookup` finders below are `Bridge`s over concrete JDK methods, so they
/// shadow the JDK's own `Lookup` bytecode in BOTH modes. Removal issue (what
/// must exist for `--jdk-only` to run the JDK's finders and intercept only
/// the handle construction):
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub fn register_p63_method_handles_lookup(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mh = "java/lang/invoke/MethodHandles";
    r.register(
        mh,
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            // WP4.2: MethodHandles.lookup() is caller-sensitive. The
            // returned Lookup must have `lookupClass` set to the caller's
            // class so downstream `findVarHandle(name, type)` and
            // `MhUtil.findVarHandle(Lookup, String, Class)` (which calls
            // `lookup.lookupClass()` internally) work correctly. Without
            // this, e.g. `CompletableFuture.<clinit>` produces a null
            // RESULT VarHandle and the subsequent `setRelease` NPEs.
            //
            // Walk the stack to find the caller of MethodHandles.lookup().
            // capture_stack_trace returns frames bottom-first (main first,
            // innermost last). The native frame itself is not included, so
            // frames[len-1] is the caller (whoever invoked lookup()).
            let frames = ctx.capture_stack_trace();
            if crate::nbflags().dbg_lookup {
                eprintln!(
                    "[DBG_LOOKUP] lookup(): {} frame(s); innermost = {:?}",
                    frames.len(),
                    frames.last().map(|f| (f.class_name.clone(), f.method_name.clone(), f.class_id)),
                );
            }
            let caller_class = if let Some(frame) = frames.last() {
                // Prefer the frame's own `class_id` (captured live from the
                // interpreter frame) over a name-based re-lookup. A name-keyed
                // lookup (`ensure_class_initialized`) is loader-blind and
                // collapses to whichever class of that name is globally
                // registered first, which is wrong whenever the actual caller
                // was loaded by a distinct ClassLoader from a same-named class
                // elsewhere on the classpath — the caller then gets handed a
                // `Lookup` whose `lookupClass` is a DIFFERENT class object
                // (defined fresh under the fallback/application namespace)
                // than the one actually executing, so `lookupClass().getClassLoader()`
                // and everything keyed on it (`Lookup.findClass`, access
                // checks, ...) observe the wrong loader identity. See
                // `StackTraceEntry::class_id`'s doc comment and
                // `deprecated_internal.rs`'s `Reflection.getCallerClass(int)`
                // for the same fix applied to the same class of bug.
                let cid = match frame.class_id {
                    Some(cid) => cid,
                    None => {
                        let class_name = frame.class_name.replace('.', "/");
                        ctx.ensure_class_initialized(&class_name)
                            .unwrap_or(cratonvm_types::ClassId::new(0))
                    }
                };
                Value::Object(Some(ctx.get_class_mirror(cid)))
            } else {
                Value::Object(None)
            };
            // WP2.3: when real JDK MethodHandles$Lookup is loaded its
            // instance fields are { lookupClass, prevLookupClass,
            // allowedModes }. We must populate lookupClass by NAME
            // because field-order varies between synthetic-only mode
            // and real-JDK mode. Field-by-name resolves to the
            // correct slot in either case.
            // Allocate with room for the real 3-field layout (lookupClass,
            // prevLookupClass, allowedModes) so the by-name `allowedModes`
            // write below actually lands.
            // GC-safety: `caller_class` is resolved ABOVE and stored BELOW,
            // with an allocation in between. See [`lk_pin`].
            let caller_pin = lk_pin(ctx, caller_class);
            let obj: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 3)?;
            let caller_class = lk_repin_read(ctx, caller_pin, caller_class);
            ctx.set_field_by_name(obj, "lookupClass", caller_class);
            ctx.set_field_by_name(obj, "prevLookupClass", Value::Object(None));
            // Slot-0 lookupClass fallback for the pure-synthetic layout.
            ctx.set_field(obj, 0, caller_class);
            if caller_pin != usize::MAX {
                ctx.unpin_native_roots(caller_pin);
            }
            // FULL power, matching HotSpot's caller-sensitive lookup():
            // PUBLIC|PRIVATE|PROTECTED|PACKAGE|MODULE|ORIGINAL = 0x5F.
            lk_write_allowed_modes(ctx, obj, 0x5F);
            if crate::nbflags().dbg_lookup {
                let cid = ctx.class_id_of_object(obj);
                eprintln!(
                    "[DBG_LOOKUP] lookup(): total_fields={} byname_allowedModes={:?} slot1={:?} slot2={:?}",
                    ctx.class_num_total_fields(cid),
                    ctx.get_field_by_name(obj, "allowedModes"),
                    ctx.get_field(obj, 1),
                    ctx.get_field(obj, 2),
                );
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mh,
        "publicLookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            // publicLookup() is NOT caller-sensitive — it always returns the
            // same singleton Lookup whose lookupClass is java.lang.Object.
            // Setting it to Object lets findVarHandle on widely visible
            // classes still resolve, while preventing access to
            // package-private members (enforced in
            // `lk_enforce_find_access` in this file, which needs no mode bit
            // for a public member of a public class and requires PRIVATE for a
            // non-public one — so dropping PUBLIC here changes nothing, while
            // KEEPING it would have skipped that gate's `modes ==
            // UNCONDITIONAL` arm entirely). `classloader::enforce_lookup_access`,
            // which this comment used to name, was never registered and was
            // deleted 2026-08-12.
            let object_cid = ctx
                .ensure_class_initialized("java/lang/Object")
                .unwrap_or(cratonvm_types::ClassId::new(0));
            let object_mirror = ctx.get_class_mirror(object_cid);
            // GC-safety: same shape as `lookup()` above -- mirror resolved, then
            // allocated, then stored. See [`lk_pin`].
            let mirror_pin = ctx.pin_native_root(object_mirror);
            let obj =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 3)?;
            let object_mirror = ctx.read_native_pin(mirror_pin, object_mirror);
            ctx.set_field_by_name(obj, "lookupClass", Value::Object(Some(object_mirror)));
            ctx.set_field(obj, 0, Value::Object(Some(object_mirror)));
            ctx.unpin_native_roots(mirror_pin);
            // JDK 9+ contract (verified against JDK 25 src.zip,
            // java.base/java/lang/invoke/MethodHandles.java):
            //
            //   public static Lookup publicLookup() { return Lookup.PUBLIC_LOOKUP; }
            //   static final Lookup PUBLIC_LOOKUP =
            //       new Lookup(Object.class, null, UNCONDITIONAL);
            //   public int lookupModes() { return allowedModes & ALL_MODES; }
            //   ALL_MODES = PUBLIC|PRIVATE|PROTECTED|PACKAGE|MODULE|
            //               UNCONDITIONAL|ORIGINAL
            //
            // so `publicLookup().lookupModes()` is exactly UNCONDITIONAL
            // (0x20) — NOT PUBLIC (0x01), and NOT PUBLIC|UNCONDITIONAL
            // (0x21). The JDK treats 0x21 as an impossible bit combination:
            // `Lookup.toString()` switches on the exact mode word, with a
            // `case UNCONDITIONAL: return cname + "/publicLookup";` arm and
            // no PUBLIC|UNCONDITIONAL arm, so 0x21 lands in the `default:`
            // branch that `assert(false)`s.
            lk_write_allowed_modes(ctx, obj, 0x20); // UNCONDITIONAL only
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(mh, "privateLookupIn", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;", |ctx, args| {
        let target = args.first().copied().unwrap_or(Value::Object(None));
        let caller = args.get(1).copied().unwrap_or(Value::Object(None));
        // Refuse BEFORE allocating anything — see [`pli_enforce`] for the
        // measured JDK 25 contract and for the one check deliberately omitted.
        pli_enforce(ctx, target, caller)?;
        // Read before the module half, which runs Java (a GC point for
        // `caller`): a TRUSTED caller returns `new Lookup(targetClass)`
        // without `newLookup`'s class check.
        let trusted = matches!(caller, Value::Object(Some(c)) if lk_read_allowed_modes(&*ctx, c) == -1);
        // The module half (`canRead`, `isOpen`; both modes since wave 47),
        // which runs Java, so the target is read back from it (interpreter
        // round i1 wave 45, lane L5; `classloader::private_lookup_in_module_refusal`). An
        // admitted target of another module also answers the caller's lookup
        // class, the JDK's `newPreviousClass` (wave 46, lane L4).
        let (target, prev) = match (target, caller) {
            (Value::Object(Some(t)), Value::Object(Some(c))) => {
                let (t, prev) = crate::classloader::private_lookup_in_module_refusal(ctx, t, c)?;
                (Value::Object(Some(t)), prev)
            }
            _ => (target, None),
        };
        // `newLookup`'s `checkUnprivilegedlookupClass`, after the module
        // question (reached only through `--add-opens` of `java.lang.invoke`).
        // Both modes since wave 48 (lane L6; HotSpot makes it in every mode).
        if !trusted {
            lk_check_unprivileged_lookup_class(ctx, target)?;
        }
        // `alloc_concurrent_synthetic` can run a moving GC, after which the
        // `target` ObjectRef the VM handed us in `args` is stale. Same fix, and
        // the same API, as `Lookup.ensureInitialized` further down; the old code
        // wrote the pre-GC ref straight into `lookupClass`.
        let pinned = match target {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        // Pinned after the target, so read back and released first (a
        // release frees every pin from its handle onward).
        let prev_pinned = prev.map(|o| (ctx.pin_native_root(o), o));
        let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 3);
        let prev = match prev_pinned {
            Some((handle, o)) => {
                let current = ctx.read_native_pin(handle, o);
                ctx.unpin_native_roots(handle);
                Some(current)
            }
            None => None,
        };
        let target = match pinned {
            Some((handle, o)) => {
                let current = ctx.read_native_pin(handle, o);
                ctx.unpin_native_roots(handle);
                Value::Object(Some(current))
            }
            None => target,
        };
        let obj = obj?;
        ctx.set_field_by_name(obj, "lookupClass", target);
        // Slot 1 of the REAL layout is the `prevLookupClass` REFERENCE. Always
        // written, so nothing downstream can read an unwritten slot as an
        // Int: null for a target of the caller's own module (the JDK's
        // same-module shape), the caller's lookup class across modules.
        ctx.set_field_by_name(obj, "prevLookupClass", Value::Object(prev));
        // Slot-0 lookupClass fallback for the pure-synthetic layout.
        ctx.set_field(obj, 0, target);
        // privateLookupIn grants full private access but drops ORIGINAL:
        // PUBLIC|PRIVATE|PROTECTED|PACKAGE|MODULE = 0x1F (incl. PACKAGE 0x08,
        // which Lookup.defineClass requires). Measured: 31 for every target the
        // JDK admits in the SAME module as the caller. Across modules (both
        // modes since wave 47) the JDK also drops MODULE: 15
        // (`L5W45PrivateLookupInOpens` `layer-open`).
        // A TRUSTED caller's answer is `new Lookup(targetClass)`, whose modes
        // are FULL_POWER_MODES, ORIGINAL included: 95 (measured with
        // `IMPL_LOOKUP`, `L4W46PrivateLookupInTrusted`; `--jdk-only` wave 46,
        // both modes since wave 48, lane L6).
        let modes = if trusted {
            0x5F
        } else if prev.is_some() {
            0x1F & !LK_MODE_MODULE
        } else {
            0x1F
        };
        if prev.is_some() && crate::jboss_jdkspecific::dbg_layer_modules() {
            eprintln!("[ACCESS-DBG] privateLookupIn across modules: modes {modes}, previous lookup class set");
        }
        lk_write_allowed_modes(ctx, obj, modes);
        Ok(Some(Value::Object(Some(obj))))
    });

    let lk = "java/lang/invoke/MethodHandles$Lookup";
    r.register(lk, "lookupClass", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(lk, "lookupModes", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let modes = lk_read_allowed_modes(ctx, this);
        if crate::nbflags().dbg_lookup {
            eprintln!(
                "[DBG_LOOKUP] lookupModes(): byname={:?} slot1={:?} slot2={:?} -> {:#x}",
                ctx.get_field_by_name(this, "allowedModes"),
                ctx.get_field(this, 1),
                ctx.get_field(this, 2),
                modes,
            );
        }
        Ok(Some(Value::Int(modes)))
    });

    r.register(
        lk,
        "ensureInitialized",
        "(Ljava/lang/Class;)Ljava/lang/Class;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let target_class = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("Lookup.ensureInitialized target class is null".to_string()),
                    }
                    .into());
                }
            };
            let class_id =
                crate::lang_class::mirror_class_id(ctx, target_class).ok_or_else(|| {
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: "Lookup.ensureInitialized target is not a Class mirror"
                            .to_string(),
                    }
                })?;
            // Family-1 stale-ObjectRef fix (2026-07-13): same defect as the
            // sibling registration in classloader.rs::lk_ensure_initialized
            // (whichever registration order wins in a given context reaches
            // this exact bug) — `ctx.initialize_class` can run `<clinit>`
            // and trigger a moving GC, so `target_class` must be rooted
            // across the call and re-read before reuse. See
            // wildfly-parallel-boot-stale-objectref-residual.md.
            let target_class_pin = ctx.pin_native_root(target_class);
            // HIB-CV-26 fix (2026-07-16): propagate the real `<clinit>`
            // failure instead of re-wrapping it as an unrecoverable
            // `VmError::Internal` — matches real JDK
            // `Lookup.ensureInitialized`, which throws
            // `ExceptionInInitializerError` for a failed initializer.
            // Interpreter round i2 wave 58, lane L3: listed under the JDK
            // frames HotSpot raises it in.
            crate::lang_class::lookup_ensure_initialized_class(ctx, class_id)?;
            let target_class = ctx.read_native_pin(target_class_pin, target_class);
            ctx.unpin_native_roots(target_class_pin);
            Ok(Some(Value::Object(Some(target_class))))
        },
    );
    // Lookup.in(targetClass) — create Lookup with reduced access for a different class
    r.register(
        lk,
        "in",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let target_class = args.get(1).copied().unwrap_or(Value::Object(None));
            // Before any mode arithmetic: `in` REJECTS a null, primitive or
            // array target. Shared with `classloader`'s copy — see
            // `lk_check_in_target` for the measured JDK behaviour and for why
            // the test cannot live in one of the two registrations only.
            crate::classloader::lk_check_in_target(ctx, target_class)?;
            // `Lookup.in` applies FOUR reductions, not two. Re-measured for
            // this lane on OpenJDK 25.0.3 (`p.LkProbe2`, receiver
            // `MethodHandles.lookup()` == 95):
            //
            //   in(LkProbe2.class)         the lookup class itself   -> 95
            //   in(LkProbe2$Nested.class)  a NESTMATE                -> 31
            //   in(p.Mate.class)           same package, other file  -> 25
            //   in(String.class)           other module              ->  1
            //   receiver 32 (publicLookup()), any target             -> 32
            //   receiver 25, same package / nestmate                 -> 25
            //   receiver 25, other module                            ->  1
            //   receiver  1, any target we model                     ->  1
            //   receiver  0, any target INCLUDING its own class      ->  0
            //
            // Three defects lived here, each measurable:
            //
            //  (a) `if prev == 0 { FULL_POWER }` — `in()` GRANTING full-power
            //      access to a lookup that had NONE. Compounded with the
            //      `Int(0)`-for-an-absent-field trap in
            //      `lk_read_allowed_modes` (fixed above), which made `prev` 0
            //      for every fabricated Lookup, so the default fired routinely
            //      rather than never.
            //  (b) no `same_class` arm — `in(lookupClass())` returns `this` in
            //      the JDK, ORIGINAL included (95), and answered 31 here.
            //  (c) `if modes == 0 { PUBLIC }` — a floor manufacturing PUBLIC
            //      where the JDK returns 0.
            //
            // And the reduction both copies of this method originally missed:
            // `VerifyAccess.isSamePackageMember`. A same-package class that is
            // not a member of the same top-level class is a "cousin" and loses
            // PRIVATE|PROTECTED, so 95 -> &0x1F = 31 -> &~6 = 25. Returning 31
            // handed a package-mate lookup PRIVATE access the JDK does not
            // grant — the direction that turns a `find*` which SHOULD raise
            // `IllegalAccessException` into a silent success. Verified: a
            // 25-mode `p.Mate` lookup is refused `private LkProbe2.secret`.
            //
            // The arithmetic and the class-relation test are `classloader`'s,
            // shared deliberately so the two competing `in` registrations (this
            // one wins in real-JDK mode, `classloader`'s in synthetic-JDK mode)
            // cannot drift apart again.
            let prev = lk_read_allowed_modes(ctx, this);
            let lookup_class = ctx.get_field(this, 0);
            // JDK 25 consults `isSamePackageMember` (and through it
            // `getEnclosingClass()`, which can throw HotSpot's
            // IncompatibleClassChangeError) only while PRIVATE survives the
            // TRUSTED, UNCONDITIONAL and module steps, and before `newLookup`'s
            // `checkUnprivilegedlookupClass` below (JIT round 21 wave 12, lane
            // reflect; `CRATONVM_LOOKUP_IN_PACKAGE_MEMBER`).
            let private_kept = prev != -1
                && prev & LK_MODE_PRIVATE != 0
                && prev & LK_MODE_UNCONDITIONAL == 0
                && !matches!(
                    lk_in_teleport(&*ctx, lookup_class, Value::Object(None), target_class),
                    Some(LkModuleMove::Across { .. })
                );
            // The walk may load an enclosing class (a GC point; JIT round 21
            // wave 16, lane lookup-in, `CRATONVM_LOOKUP_IN_LOADS_OUTER`, page
            // `r21-review-w10-findings-FIXED-20261020.md` residual 3R): `this`,
            // `lookup_class` and `target_class` are read after it, so they
            // are its roots and are re-read from them.
            let mut roots = [Value::Object(Some(this)), lookup_class, target_class];
            let (same_class, same_package, same_nest) =
                crate::classloader::lk_class_relation_rooted(
                    ctx,
                    lookup_class,
                    target_class,
                    private_kept,
                    &mut roots,
                )?;
            let this = match roots[0] {
                Value::Object(Some(o)) => o,
                _ => this,
            };
            let (lookup_class, target_class) = (roots[1], roots[2]);
            // `if (requestedLookupClass == this.lookupClass) return this;`
            // (JDK 25): the SAME object, ORIGINAL included. A copy answered
            // `false` to `l.in(l.lookupClass()) == l`. Wave 43, lane L4.
            //
            // Before it, JDK 25's `if (allowedModes == TRUSTED) return new
            // Lookup(requestedLookupClass, null, FULL_POWER_MODES);`: a new
            // lookup, modes 95, for any target (`--jdk-only` wave 46, both
            // modes since wave 48, lane L6; `L4W46PrivateLookupInTrusted` row
            // `in-trusted-jli`).
            let trusted_in = prev == -1;
            if same_class && !trusted_in {
                return Ok(Some(Value::Object(Some(this))));
            }
            // `newLookup`'s `checkUnprivilegedlookupClass`, which every
            // non-TRUSTED `in` reaches (wave 46; both modes since wave 48).
            if prev != -1 {
                lk_check_unprivileged_lookup_class(ctx, target_class)?;
            }
            // A Lookup reporting 0 has no modes to narrow, and `in()` never
            // GRANTS. Measured: `lookup().dropLookupMode(PUBLIC)` is 0 and its
            // `.in(<own class>)` / `.in(<package mate>)` / `.in(String.class)`
            // are all 0.
            let target_is_public = match target_class {
                Value::Object(Some(m)) => crate::lang_class::mirror_is_public(ctx, m),
                _ => false,
            };
            // The class this lookup teleported from (`prevLookupClass`), and
            // whether the target is in another module (wave 43, lane L4).
            let plc = match ctx.get_field_by_name(this, "prevLookupClass") {
                v @ Value::Object(Some(_)) => v,
                _ => Value::Object(None),
            };
            let unconditional = prev & LK_MODE_UNCONDITIONAL != 0;
            let teleport = if unconditional {
                None
            } else {
                lk_in_teleport(ctx, lookup_class, plc, target_class)
            };
            let (modes, new_plc) = if trusted_in {
                (LK_MODE_FULL_POWER | 0x40, Value::Object(None))
            } else {
                match teleport {
                    // The receiver is `publicLookup()`-like, or this context
                    // cannot tell the modules apart: the package-based reductions
                    // as before, `prevLookupClass` kept.
                    None => {
                        let mut modes = if prev == 0 {
                            0
                        } else {
                            crate::classloader::lk_in_modes(
                                prev,
                                same_class,
                                same_package,
                                same_nest,
                                target_is_public,
                            )
                        };
                        // `VerifyAccess.isClassAccessible`, the last reduction:
                        // a class that is not public is accessible only to a
                        // PACKAGE lookup of its own package (the UNCONDITIONAL arm
                        // already applies its own rule in `lk_in_modes`).
                        if !unconditional
                            && modes & (LK_MODE_PUBLIC | LK_MODE_UNCONDITIONAL) != 0
                            && !lk_in_class_accessible(prev, same_package, target_is_public)
                        {
                            modes = 0;
                        }
                        let plc = if unconditional {
                            Value::Object(None)
                        } else {
                            plc
                        };
                        (modes, plc)
                    }
                    Some(teleport) => {
                        // `isClassAccessible` against the RECEIVER's lookup class,
                        // previous lookup class and modes, modules included; the
                        // package-only answer when a module cannot be read.
                        let accessible = lk_class_accessible_in_modules(
                            &*ctx,
                            prev,
                            same_package,
                            target_is_public,
                            lookup_class,
                            plc,
                            target_class,
                        )
                        .unwrap_or_else(|| {
                            lk_in_class_accessible(prev, same_package, target_is_public)
                        });
                        match teleport {
                            LkModuleMove::Same => (
                                lk_in_modes_same_module(prev, same_package, same_nest, accessible),
                                plc,
                            ),
                            LkModuleMove::Across { third } => (
                                lk_in_modes_across_modules(prev, third, accessible),
                                lookup_class,
                            ),
                        }
                    }
                }
            };
            // GC-safety: `target_class` came out of `args` and every read of
            // it since has been of that SNAPSHOT, which the allocation below
            // can leave pointing at a pre-GC address. See [`lk_pin`]. The
            // same for the class the new lookup records as its previous one.
            let target_pin = lk_pin(ctx, target_class);
            let plc_pin = lk_pin(ctx, new_plc);
            let lookup: ObjectRef =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 3)?;
            let target_class = lk_repin_read(ctx, target_pin, target_class);
            let new_plc = lk_repin_read(ctx, plc_pin, new_plc);
            ctx.set_field_by_name(lookup, "lookupClass", target_class);
            ctx.set_field(lookup, 0, target_class); // lookupClass = targetClass
                                                    // Written in every case, so `previousLookupClass()` (the JDK's
                                                    // own field read) never meets an unwritten slot; `privateLookupIn`
                                                    // does the same.
            ctx.set_field_by_name(lookup, "prevLookupClass", new_plc);
            // Unpinned in reverse order: `plc_pin` above `target_pin`.
            if plc_pin != usize::MAX {
                ctx.unpin_native_roots(plc_pin);
            }
            if target_pin != usize::MAX {
                ctx.unpin_native_roots(target_pin);
            }
            lk_write_allowed_modes(ctx, lookup, modes);
            Ok(Some(Value::Object(Some(lookup))))
        },
    );

    // findVirtual — resolve to real 5-field MH with descriptor from MethodType
    r.register(lk, "findVirtual",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_virtual);

    // findStatic — resolve to real 5-field MH
    r.register(lk, "findStatic",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_static);

    // findConstructor — resolve to real 5-field MH
    r.register(
        lk,
        "findConstructor",
        "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_constructor,
    );

    // findSpecial — same as findVirtual but MH_KIND_SPECIAL
    r.register(lk, "findSpecial",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_special);

    // findGetter — creates a MH with kind=GETTER that reads an instance field
    r.register(
        lk,
        "findGetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_getter,
    );

    // findSetter — creates a MH with kind=SETTER that writes an instance field
    r.register(
        lk,
        "findSetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_setter,
    );

    // findStaticGetter/findStaticSetter
    r.register(
        lk,
        "findStaticGetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_static_getter,
    );
    r.register(
        lk,
        "findStaticSetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_static_setter,
    );

    // findVarHandle — create a real VarHandle for an instance field
    r.register(
        lk,
        "findVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        lookup_find_var_handle,
    );

    // findStaticVarHandle
    r.register(
        lk,
        "findStaticVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        lookup_find_static_var_handle,
    );

    // revealDirect — crack a (direct) MethodHandle into a MethodHandleInfo.
    //
    // The default JDK path calls `mh.isCrackable()` and `mh.internalMemberName()`,
    // which only return useful values on `DirectMethodHandle`. Our `findStatic` /
    // `findVirtual` / `findGetter` / etc. allocate plain MethodHandle objects with
    // our private synthetic slots populated (MH_CLASS, MH_NAME, MH_DESC, MH_KIND),
    // so the default path throws `IllegalArgumentException: not a direct method
    // handle`. This native rebuilds a fully populated MemberName from our slots
    // and wraps it in an `InfoFromMemberName`, which is what the JDK path would
    // have returned for a real DirectMethodHandle.
    //
    // This is the entry point cracked by LambdaMetafactory (via
    // `caller.revealDirect(implementation)`), so getting this right is what lets
    // log4j 2.x's `ServiceLoaderUtil$LazyProviderHolder.<clinit>` lambda chain
    // succeed in real-JDK mode.
    // `MethodHandle.internalMemberName()` of this model's FIELD handles
    // (interpreter round i1 wave 49, lane L4;
    // `interpreter-L4-constantbootstraps-getstaticfinal-reads-no-membername-FIXED-20261013`): the
    // member a real `DirectMethodHandle` holds. `ConstantBootstraps.getStaticFinal`
    // reads its `isFinal()`. A real JDK handle's override still wins by
    // virtual dispatch; every other receiver runs the base class's bytecode
    // (`null`). A `Bridge` in this block's scope.
    r.register(
        "java/lang/invoke/MethodHandle",
        "internalMemberName",
        "()Ljava/lang/invoke/MemberName;",
        method_handle_internal_member_name,
    );
    r.register(
        lk,
        "revealDirect",
        "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandleInfo;",
        lookup_reveal_direct,
    );

    // JDK 25 `Lookup.toString()`: the lookup class's name, `/` and the
    // previous lookup class's name when it teleported, then a suffix chosen by
    // the EXACT mode word. It printed the fixed text `MethodHandles.Lookup`
    // (interpreter round i1 wave 43, lane L4; host run of `L4W43LookupTeleport`).
    r.register(lk, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let text = lookup_to_string(ctx, this);
        let s = ctx.create_string_uninterned(&text);
        Ok(Some(Value::Object(Some(s))))
    });
    // Interpreter round i3 wave 77, lane L4 (`CRATONVM_MH_FINDER_MINT_MEMO`,
    // default off): in a `--jdk-only` VM over a real JDK without a JIT, the
    // four field finders above are replaced by reviewed `Intrinsic`s (a memo
    // over the JDK's own body; see `register_finder_mint_memo`). After the
    // carriers, so the last write wins; a no-op in every other VM. Removal:
    // when the interpreter runs the finder's body cheaply enough that the
    // `find-*` rows of `L4W77FinderMintBench` show no gain.
    register_finder_mint_memo(r);
    // Interpreter round i3 wave 82, lane L4
    // (`CRATONVM_MH_METHOD_FINDER_MINT_MEMO`, default off): the same memo over
    // the five METHOD finders (`findVirtual`, `findStatic`, `findSpecial`,
    // `findConstructor`, `unreflect`); see `register_method_finder_mint_memo`.
    // Kinds stated per row; a no-op in every VM the switch does not admit.
    // Removal: when the `find-*` rows of `L4W81MethodFinderBench` show no gain.
    register_method_finder_mint_memo(r);
    r.set_category(__prev_cat);
}

/// `MethodHandles.Lookup.toString()` (JDK 25), transcribed: `cname` is the
/// lookup class's `getName()`, then `/<previous lookup class>` when there is
/// one, then the suffix of the exact `allowedModes` word. A class name this
/// context cannot read prints as `MethodHandles.Lookup` (the old fixed text).
pub(crate) fn lookup_to_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let name_of = |ctx: &dyn NativeContext, v: Value| match v {
        Value::Object(Some(m)) => {
            crate::lang_class::mirror_class_name(ctx, m).map(|n| n.class_text(&*ctx))
        }
        _ => None,
    };
    let mut cname = name_of(&*ctx, ctx.get_field(this, 0))
        .unwrap_or_else(|| "MethodHandles.Lookup".to_string());
    if let Some(prev) = name_of(&*ctx, ctx.get_field_by_name(this, "prevLookupClass")) {
        cname.push('/');
        cname.push_str(&prev);
    }
    const PUBLIC: i32 = LK_MODE_PUBLIC;
    const PRIVATE: i32 = LK_MODE_PRIVATE;
    const PROTECTED: i32 = LK_MODE_PROTECTED;
    const PACKAGE: i32 = LK_MODE_PACKAGE;
    const MODULE: i32 = LK_MODE_MODULE;
    const ORIGINAL: i32 = 0x40;
    let modes = lk_read_allowed_modes(&*ctx, this);
    match modes {
        0 => format!("{cname}/noaccess"),
        LK_MODE_UNCONDITIONAL => format!("{cname}/publicLookup"),
        PUBLIC => format!("{cname}/public"),
        m if m == PUBLIC | MODULE => format!("{cname}/module"),
        m if m == PUBLIC | PACKAGE || m == PUBLIC | MODULE | PACKAGE => format!("{cname}/package"),
        m if m == PUBLIC | PACKAGE | PRIVATE || m == PUBLIC | MODULE | PACKAGE | PRIVATE => {
            format!("{cname}/private")
        }
        m if m == PUBLIC | PACKAGE | PRIVATE | PROTECTED
            || m == PUBLIC | MODULE | PACKAGE | PRIVATE | PROTECTED
            || m == LK_MODE_FULL_POWER | ORIGINAL =>
        {
            cname
        }
        -1 => "/trusted".to_string(),
        other => format!("{cname}/{other:x}"),
    }
}

// ---------------------------------------------------------------------------
// Lookup.find* implementations
// ---------------------------------------------------------------------------

pub(crate) fn lookup_find_virtual(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // FIRST statement on purpose: `args` still holds the ObjectRefs the VM
    // handed us and nothing below has had a chance to allocate and move them.
    lk_enforce_find_access(ctx, args, 2, None, false, lk_virtual_ref_kind(ctx, args))
        .map_err(|r| r.into_error(ctx))?;
    // Wave 81: another nest's private method.
    let virtual_ref_kind = lk_virtual_ref_kind(ctx, args);
    // Its refreshed copy of `args` when it loaded a nest host (a GC point).
    let refreshed = lk_enforce_private_nest_access(ctx, args, 2, None, virtual_ref_kind)?;
    let args: &[Value] = refreshed.as_deref().unwrap_or(args);
    // A null argument is a CALLER BUG, not an absent member -- see
    // `lookup_null_arg` for why answering `NoSuchMethodException` here misled
    // the version probes this method exists to serve.
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let mt_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    if let Some(proxy) = lk_lambda_proxy_of_mirror(&*ctx, class_obj) {
        return lk_lambda_proxy_find(ctx, proxy, name_obj, mt_obj, "invokeVirtual");
    }
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let desc = descriptor_from_method_type(ctx, mt_obj);
    // Read before the load below, a GC point for both references.
    let strict = mhn_strict_method_descriptor(&*ctx, mt_obj);
    let class_id = ctx.class_id_from_mirror(class_obj);
    let from = lk_from_of(&*ctx, args);
    // LOAD, never initialize (see `lookup_load_class`).
    let loaded = lookup_load_class(ctx, &class).is_ok();
    // `Lookup.findVirtual` asks with `REF_invokeInterface` when `refc` is an
    // interface; the kind is part of HotSpot's refusal text.
    let ref_kind = if class_id.is_some_and(|cid| ctx.is_interface_class(cid)) {
        "invokeInterface"
    } else {
        "invokeVirtual"
    };
    // A STATIC method is `IllegalAccessException: no such method` (wave 24).
    lk_check_method(
        &*ctx,
        loaded,
        class_id,
        &class,
        &name,
        &desc,
        strict.as_deref(),
        ref_kind,
        false,
    )?;
    // A protected method of another package: the receiver is the lookup
    // class (`getDirectMethodCommon`; wave 44, `L4W44ProtectedReceiver`).
    let restricted = lk_protected_receiver(&*ctx, from, class_id, LkMember::Method(&name, &desc));
    let mh = alloc_method_handle_with_receiver(
        ctx,
        &class,
        &name,
        &desc,
        MH_KIND_VIRTUAL,
        restricted.as_deref(),
        class_id,
    )?;
    Ok(Some(Value::Object(Some(mh))))
}

// ---------------------------------------------------------------------------
// Members of a lambda proxy class (interpreter round i2 wave 61, lane L4)
// ---------------------------------------------------------------------------
//
// A lambda proxy is minted outside the class store: its mirror has no class
// name (`mirror_class_name` answers `None`), so every finder above built the
// handle over the class `""` -- one whose `type()` had NO receiver parameter
// (`alloc_method_handle_with_receiver` prepends `L<class>;` only for a named
// class), and whose member was never checked. HotSpot answers the spun hidden
// class's own member: `findVirtual(P, "writeReplace", ()Object).type()` is
// `(P)Object`. The JDK's reflective accessor
// (`MethodHandleAccessorFactory.getDirectMethod`, the path
// `CRATONVM_JDK_REFLECTIVE_INVOKE` runs) then failed `asType((Object)Object)`
// with `WrongMethodTypeException`, so no serializable lambda could be
// serialized under it
// (`docs/feature-designs/native-dispatch-architecture.md`).
//
// The finders now resolve the member among the proxy's methods (its own, as
// `getDeclaredMethods` lists them, then `Object`'s, then its interfaces'
// instance methods), refuse an absent one with HotSpot's text, and mint a
// VIRTUAL handle over the class `""` -- the dispatch calls the member on the
// receiver, which is how a proxy's methods have always been called -- typed
// `(P, ..)R` with the proxy's own mirror leading. A proxy is final and
// declares no static method, so `findSpecial` / `unreflectSpecial` answer the
// same handle. Probe `tools/probes/interp/L4/L4W61LambdaProxyMemberHandles.java`;
// kill switch `CRATONVM_MH_LAMBDA_PROXY_MEMBERS=0`.
//
// The field finders (`findGetter`, `findSetter`, `unreflectGetter`,
// `unreflectSetter`) had the same gap over a proxy's captured `arg$N` fields:
// a getter typed `(L;)T` with a stand-in mirror (`type().toString()` raised
// `NullPointerException` under `--jdk-only`, `(?)T` under `--compatible`),
// and no refusal at all. They resolve the capture by name and type now, refuse
// a setter (a hidden class's finals are trusted), and type the receiver with
// the proxy's mirror. Probe `L4W61LambdaProxyFieldHandles`. A `VarHandle` over
// a capture (`findVarHandle`) is not covered.

/// `Class.getName()` of `target` for a `Lookup` refusal's text: a lambda
/// proxy's own name (its `/0x..` suffix kept), else the binary name, `?` when
/// the mirror names no class (interpreter round i2 wave 61, lane L4; a proxy
/// printed `?`).
pub(crate) fn lk_refusal_class_name(ctx: &dyn NativeContext, target: ObjectRef) -> String {
    if let Some((_, name)) = lk_lambda_proxy_of_mirror(ctx, target) {
        return name;
    }
    mirror_class_name(ctx, target)
        .unwrap_or_else(|| "?".to_string())
        .class_text(&*ctx)
}

/// The lambda proxy class `mirror` names, with its `Class.getName()` (the
/// spelling HotSpot's refusal texts use), or `None` for any other class.
pub(crate) fn lk_lambda_proxy_of_mirror(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
) -> Option<(ClassId, String)> {
    if !crate::nbflags().mh_lambda_proxy_members {
        return None;
    }
    let cid = ctx.class_id_from_mirror(mirror)?;
    let name = crate::lang_class::lambda_proxy_class_name(ctx, cid)?;
    Some((cid, name))
}

/// JVMS 5.4.3.3 resolution of `name` + `desc` from lambda proxy `proxy`: the
/// proxy's own methods (`getDeclaredMethods`' list), then `Object`'s, then
/// the instance methods of its functional interface and marker interfaces
/// (and their superinterfaces). The found method's access flags, or `None`.
pub(crate) fn lk_lambda_proxy_method_access(
    ctx: &dyn NativeContext,
    proxy: ClassId,
    name: &str,
    desc: &str,
) -> Option<u16> {
    let own = crate::lang_class::declared_methods_with_synthetic(ctx, proxy);
    if let Some(m) = own.iter().find(|m| m.name == name && m.descriptor == desc) {
        return Some(m.access_flags);
    }
    if let Some(object) = ctx.class_id_by_name("java/lang/Object") {
        if let Some(flags) = lk_declared_method_flags(ctx, object, name, desc)
            .filter(|flags| flags & MHN_ACC_PRIVATE == 0)
        {
            return Some(flags);
        }
    }
    let meta = ctx.lambda_proxy_serial_metadata(proxy)?;
    for iface in std::iter::once(&meta.functional_interface).chain(meta.marker_interfaces.iter()) {
        let declared = ctx.class_id_by_name(iface).and_then(|id| {
            lk_declared_method_flags(ctx, id, name, desc)
                .filter(|flags| flags & (MHN_ACC_STATIC | MHN_ACC_PRIVATE) == 0)
        });
        if declared.is_some() {
            return declared;
        }
        if lk_superinterface_declares_instance_method(ctx, iface, name, desc) {
            return Some(0x0001);
        }
    }
    None
}

/// `Class.getName()`-spelled member text of a lambda proxy's method:
/// [`lk_method_member_display`] without its `/` -> `.` rewrite, which would
/// mangle the hidden class's `/0x` suffix.
pub(crate) fn lk_lambda_proxy_member_display(
    proxy_name: &str,
    name: &str,
    desc: &str,
    ref_kind: &str,
) -> String {
    let ty = method_type_display(desc).unwrap_or_else(|| desc.to_string());
    format!("{proxy_name}.{name}{ty}/{ref_kind}")
}

/// `findVirtual` / `findSpecial` / `findStatic` (`ref_kind`) of `name_obj` +
/// `mt_obj` on lambda proxy `proxy`: HotSpot's refusals (an absent member is
/// `NoSuchMethodException`, one of the other static-ness
/// `IllegalAccessException`, both `no such method: P.m()R/kind`), else the
/// member's handle ([`alloc_lambda_proxy_member_handle`]). Reads both
/// arguments before anything allocates.
pub(crate) fn lk_lambda_proxy_find(
    ctx: &mut dyn NativeContext,
    proxy: (ClassId, String),
    name_obj: ObjectRef,
    mt_obj: ObjectRef,
    ref_kind: &str,
) -> MethodCallResult {
    let (proxy_id, proxy_name) = proxy;
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let desc = descriptor_from_method_type(&*ctx, mt_obj);
    let want_static = ref_kind == "invokeStatic";
    match lk_lambda_proxy_method_access(&*ctx, proxy_id, &name, &desc) {
        None => Err(RuntimeError::NoSuchMethodException {
            message: format!(
                "no such method: {}",
                lk_lambda_proxy_member_display(&proxy_name, &name, &desc, ref_kind)
            ),
        }
        .into()),
        Some(access) if (access & MHN_ACC_STATIC != 0) != want_static => {
            Err(lookup_access_error(format!(
                "no such method: {}",
                lk_lambda_proxy_member_display(&proxy_name, &name, &desc, ref_kind)
            )))
        }
        Some(_) => {
            let mh = alloc_lambda_proxy_member_handle(ctx, proxy_id, &name, &desc)?;
            Ok(Some(Value::Object(Some(mh))))
        }
    }
}

/// `unreflect` / `unreflectSpecial` of `method_obj` when it is a lambda
/// proxy's method (its `clazz` is a proxy's mirror): the member's handle, as
/// [`lk_lambda_proxy_find`] mints it. `None` for any other `Method`.
pub(crate) fn lk_lambda_proxy_unreflect(
    ctx: &mut dyn NativeContext,
    method_obj: ObjectRef,
) -> Option<MethodCallResult> {
    let Value::Object(Some(clazz)) = ctx.get_field_by_name(method_obj, "clazz") else {
        return None;
    };
    let (proxy_id, _) = lk_lambda_proxy_of_mirror(&*ctx, clazz)?;
    let name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let desc = crate::lang_class::read_method_descriptor(ctx, method_obj).unwrap_or_default();
    Some(
        alloc_lambda_proxy_member_handle(ctx, proxy_id, &name, &desc)
            .map(|mh| Some(Value::Object(Some(mh)))),
    )
}

/// The handle over instance method `name` + `desc` of lambda proxy `proxy`: a
/// VIRTUAL handle over the class `""` (its dispatch calls the member on the
/// receiver), whose `type` is `desc`'s with the proxy's mirror leading.
pub(crate) fn alloc_lambda_proxy_member_handle(
    ctx: &mut dyn NativeContext,
    proxy: ClassId,
    name: &str,
    desc: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let mh = alloc_method_handle(ctx, "", name, desc, MH_KIND_VIRTUAL)?;
    let mh = lk_lambda_proxy_retype(ctx, mh, proxy, false)?;
    // The receiver cast's owner (wave 62, `CRATONVM_MH_INSTANCE_OWNER_ID`):
    // the class `""` cast nothing.
    mh_record_instance_owner(&*ctx, mh, Some(proxy));
    Ok(mh)
}

/// [`mh_lead_type_with_class`] on the fresh handle `mh`, which it answers at
/// its current address.
pub(crate) fn lk_lambda_proxy_retype(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    proxy: ClassId,
    replace_first: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let mh_pin = ctx.pin_native_root(mh);
    let typed = mh_lead_type_with_class(ctx, mh_pin, mh, proxy, replace_first);
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.unpin_native_roots(mh_pin);
    typed?;
    Ok(mh)
}

/// The field getter (`setter` false) or setter over captured field `name`
/// (`field_desc`) of lambda proxy `proxy`: HotSpot's refusals -- a field of
/// no such name and type is `NoSuchFieldException: no such field:
/// P.f/T/getField`, and a capture is final in a hidden class, so a setter is
/// `IllegalAccessException` (`final_refusal`: `findSetter`'s "unexpected set
/// of a final field", `unreflectSetter`'s "final field has no write access",
/// each with the lookup's `, from ..` tail; a TRUSTED lookup's `findSetter`
/// is admitted) -- else a GETTER / SETTER over the class `""` (the door reads
/// the field off the receiver) typed with the proxy's mirror for the
/// receiver. Interpreter round i2 wave 61, lane L4
/// (`L4W61LambdaProxyFieldHandles`).
pub(crate) fn lk_lambda_proxy_field_handle(
    ctx: &mut dyn NativeContext,
    proxy: (ClassId, String),
    name: &str,
    field_desc: &str,
    setter: bool,
    final_refusal: &str,
    trusted_admits: bool,
    from: LkFrom,
) -> MethodCallResult {
    let (proxy_id, proxy_name) = proxy;
    let ref_kind = if setter { "putField" } else { "getField" };
    let shown = format!(
        "{proxy_name}.{name}/{}/{ref_kind}",
        mh_class_get_name(field_desc)
    );
    let field = crate::lang_class::declared_fields_with_aliases(&*ctx, proxy_id)
        .into_iter()
        .find(|f| !f.is_static && f.name == name && f.descriptor == field_desc);
    let Some(field) = field else {
        return Err(RuntimeError::NoSuchFieldException {
            field_name: format!("no such field: {shown}"),
        }
        .into());
    };
    if setter && field.access_flags & LK_ACC_FINAL != 0 && !(trusted_admits && from.is_trusted()) {
        let from = lk_from_suffix(ctx, from);
        return Err(lookup_access_error(format!(
            "{final_refusal}: {shown}{from}"
        )));
    }
    let (desc, kind) = if setter {
        (format!("(L;{field_desc})V"), MH_KIND_SETTER)
    } else {
        (format!("(L;){field_desc}"), MH_KIND_GETTER)
    };
    let mh = alloc_method_handle(ctx, "", name, &desc, kind)?;
    let mh = lk_lambda_proxy_retype(ctx, mh, proxy_id, true)?;
    mh_record_instance_owner(&*ctx, mh, Some(proxy_id));
    Ok(Some(Value::Object(Some(mh))))
}

/// `findVarHandle(P, name, type)` over captured field `name` (`field_desc`) of
/// lambda proxy `proxy` (interpreter round i3 wave 79, lane L4; page
/// `i61-L4-lambda-proxy-handles-check-no-receiver-and-do-not-crack`,
/// `--compatible`): HotSpot's `NoSuchFieldException: no such field:
/// P.f/T/getField` for a field of no such name and type, else an instance
/// `VarHandle` whose coordinate is the proxy itself (its internal name and
/// `ClassId`, the capture's slot), READ-ONLY: a capture is `final` in a hidden
/// class, so HotSpot answers a `FieldInstanceReadOnly`. `coordinateTypes()`
/// names the proxy's mirror ([`vh_lambda_proxy_coordinate_types`]) and the
/// receiver cast admits only the proxy's own instances
/// ([`vh_instance_refusal`]).
pub(crate) fn lk_lambda_proxy_var_handle(
    ctx: &mut dyn NativeContext,
    proxy: (ClassId, String),
    name: &str,
    field_desc: &str,
) -> MethodCallResult {
    let (proxy_id, proxy_name) = proxy;
    let shown = format!(
        "{proxy_name}.{name}/{}/getField",
        mh_class_get_name(field_desc)
    );
    let field = crate::lang_class::declared_fields_with_aliases(&*ctx, proxy_id)
        .into_iter()
        .find(|f| !f.is_static && f.name == name && f.descriptor == field_desc);
    let Some(field) = field else {
        return Err(RuntimeError::NoSuchFieldException {
            field_name: format!("no such field: {shown}"),
        }
        .into());
    };
    let slot = i32::try_from(field.slot_index).unwrap_or(-1);
    let internal = proxy_name.replace('.', "/");
    let vh = alloc_instance_var_handle(
        ctx,
        &internal,
        name,
        field_desc,
        slot,
        proxy_id,
        Some(proxy_id),
    )?;
    let vh = vh_mark_read_only(ctx, vh);
    Ok(Some(Value::Object(Some(vh))))
}

/// `findGetter` / `findSetter` (`setter`) of `name_obj` typed `type_obj` on
/// lambda proxy `proxy` ([`lk_lambda_proxy_field_handle`]). Reads `args` and
/// both references before anything allocates.
pub(crate) fn lk_lambda_proxy_field_find(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    proxy: (ClassId, String),
    name_obj: ObjectRef,
    type_obj: ObjectRef,
    setter: bool,
) -> MethodCallResult {
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(&*ctx, type_obj).into_owned();
    let from = lk_from_of(&*ctx, args);
    lk_lambda_proxy_field_handle(
        ctx,
        proxy,
        &name,
        &field_desc,
        setter,
        "unexpected set of a final field",
        true,
        from,
    )
}

/// `unreflectGetter` / `unreflectSetter` of `field_obj` when it is a lambda
/// proxy's captured field ([`lk_lambda_proxy_field_handle`]); `None` for any
/// other `Field`.
pub(crate) fn lk_lambda_proxy_unreflect_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field_obj: ObjectRef,
    setter: bool,
) -> Option<MethodCallResult> {
    let Value::Object(Some(clazz)) = ctx.get_field_by_name(field_obj, "clazz") else {
        return None;
    };
    let proxy = lk_lambda_proxy_of_mirror(&*ctx, clazz)?;
    let name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let field_desc = read_field_descriptor_string(ctx, field_obj).into_owned();
    let from = lk_from_of(&*ctx, args);
    Some(lk_lambda_proxy_field_handle(
        ctx,
        proxy,
        &name,
        &field_desc,
        setter,
        "final field has no write access",
        false,
        from,
    ))
}

/// Restamp the `type` of the handle pinned at `mh_pin` with `lead`'s mirror
/// as its first parameter -- inserted (`MethodType.insertParameterTypes(0,
/// lead)`), or in place of the first one (`replace_first`, a field handle
/// minted with a stand-in receiver) -- the JDK's interned instance when it
/// can answer one ([`intern_descriptor_type_via_jdk`]), else a native
/// carrier, as [`build_method_type_from_descriptor`] builds them. A handle
/// with no readable `type` is left as it is. Leaves every pin it takes above
/// `mh_pin` for the caller's unpin.
pub(crate) fn mh_lead_type_with_class(
    ctx: &mut dyn NativeContext,
    mh_pin: usize,
    mh: ObjectRef,
    lead: ClassId,
    replace_first: bool,
) -> Result<(), MethodCallFailed> {
    let mh = ctx.read_native_pin(mh_pin, mh);
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return Ok(());
    };
    let Some(rtype) = recover_class_mirror_from_slot(&*ctx, ctx.get_field(mt, 0)) else {
        return Ok(());
    };
    let Value::Object(Some(ptypes)) = ctx.get_field(mt, 1) else {
        return Ok(());
    };
    let count = ctx.array_length(ptypes);
    let skip = usize::from(replace_first).min(count);
    let rtype_pin = ctx.pin_native_root(rtype);
    let ptypes_pin = ctx.pin_native_root(ptypes);
    let lead_mirror = ctx.get_class_mirror(lead);
    let lead_pin = ctx.pin_native_root(lead_mirror);
    let arr = new_class_array(ctx, count - skip + 1);
    let arr_pin = ctx.pin_native_root(arr);
    let lead_mirror = ctx.read_native_pin(lead_pin, lead_mirror);
    let ptypes = ctx.read_native_pin(ptypes_pin, ptypes);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_array_element(arr, 0, Value::Object(Some(lead_mirror)));
    for i in skip..count {
        let p = ctx.get_array_element(ptypes, i);
        ctx.set_array_element(arr, i - skip + 1, p);
    }
    let rtype = ctx.read_native_pin(rtype_pin, rtype);
    let new_type = match intern_descriptor_type_via_jdk(ctx, rtype_pin, rtype, arr_pin, arr) {
        Some(t) => t,
        None => {
            let t = try_alloc_concurrent_synthetic(ctx, MT_CLASS, 6)?;
            let rtype = ctx.read_native_pin(rtype_pin, rtype);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(t, 0, Value::Object(Some(rtype)));
            ctx.set_field(t, 1, Value::Object(Some(arr)));
            let mut t = t;
            populate_method_type_form(ctx, &mut t)?;
            t
        }
    };
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.set_field_by_name(mh, "type", Value::Object(Some(new_type)));
    Ok(())
}

/// The resolution check a METHOD finder owes, HotSpot's refusal texts
/// included (wave 24; see the block above [`lk_method_member_display`]): the
/// proven answer of [`lk_method_resolution`] when there is one, otherwise the
/// older evidence rules ([`lookup_require_method`], [`lk_method_is_static`]).
/// A method of the other static-ness than `want_static` is refused.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lk_check_method(
    ctx: &dyn NativeContext,
    loaded: bool,
    class_id: Option<ClassId>,
    class: &str,
    name: &str,
    desc: &str,
    strict: Option<&str>,
    ref_kind: &str,
    want_static: bool,
) -> Result<(), MethodCallFailed> {
    match lk_method_resolution(ctx, class_id, class, name, desc, strict) {
        MhnVerdict::Absent => Err(lk_no_such_method(class, name, desc, ref_kind)),
        MhnVerdict::Found(access) => {
            if (access & MHN_ACC_STATIC != 0) != want_static {
                return Err(lk_method_static_mismatch(class, name, desc, ref_kind));
            }
            Ok(())
        }
        MhnVerdict::Unknown => {
            lookup_require_method(ctx, loaded, class, name, desc, ref_kind, !want_static)?;
            // `None` is unknown and an accept.
            if lk_method_is_static(ctx, class, name, desc) == Some(!want_static) {
                return Err(lk_method_static_mismatch(class, name, desc, ref_kind));
            }
            Ok(())
        }
    }
}

/// `Lookup.findVirtual`/`findStatic`/`findSpecial` must raise
/// `NoSuchMethodException` when the member is absent — a *checked* exception.
///
/// Handing back a `MethodHandle` regardless and letting the call site raise
/// `NoSuchMethodError` at invoke time is not a smaller version of the same
/// behaviour: `NoSuchMethodError` extends `Error`, so it sails straight through
/// the `catch (Exception)` that every version-probing library writes. H2's
/// `FullTextLucene.<clinit>` is the canonical shape —
///
/// ```java
/// try   { mh = lookup.findVirtual(TotalHits.class, "value", methodType(long.class)); }
/// catch (Exception e) { mh = lookup.findGetter(TotalHits.class, "value", long.class); }
/// ```
///
/// — Lucene 10 exposes `value()`, Lucene 9 only the `value` field, and with an
/// `Error` escaping the probe H2 died with `NoSuchMethodError: TotalHits.value()J`
/// instead of taking its own fallback.
///
/// The escape hatch that motivated the old "create it anyway" comment is kept
/// where it belongs: `method_exists` already answers true for synthetic-stub
/// classes and for anything in the native registry, so a false answer means the
/// member genuinely is not there. Only when the class itself could not be
/// initialised do we stay quiet and let dispatch decide.
///
/// `instance` is set by the finders that resolve an INSTANCE method
/// (`findVirtual`, `findSpecial`): JVMS §5.4.3.3 / §5.4.3.4 then also find a
/// non-static, non-private method declared in a SUPERINTERFACE, which is how a
/// default method a class inherits (and an interface method declared in a
/// superinterface) resolves. `method_exists` walks superclasses only, so
/// `findVirtual(Impl.class, "defaultMethod", ..)` answered
/// `NoSuchMethodException` where HotSpot hands back the handle (round 12 wave 2
/// hunter, `R12HuntMethodHandles`). `findStatic` passes `false`: a static
/// interface method is never inherited.
pub(crate) fn lookup_require_method(
    ctx: &dyn NativeContext,
    class_loaded: bool,
    class: &str,
    name: &str,
    desc: &str,
    ref_kind: &str,
    instance: bool,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if class_loaded
        && !ctx.method_exists(class, name, desc)
        && !(instance && lk_superinterface_declares_instance_method(ctx, class, name, desc))
    {
        return Err(lk_no_such_method(class, name, desc, ref_kind));
    }
    Ok(())
}

pub(crate) const LK_ACC_PRIVATE: u16 = 0x0002;

/// Does any superinterface of `class` -- of the class itself or of any class
/// on its superclass chain, transitively -- declare a non-static, non-private
/// `name desc`? The superinterface step of JVMS §5.4.3.3 (method resolution)
/// and §5.4.3.4 (interface method resolution); the superclass step is
/// `method_exists`. Abstract declarations count: resolution finds them too.
///
/// Bounded like [`lk_method_is_static`], so a malformed or self-referential
/// fabricated hierarchy cannot spin here.
pub(crate) fn lk_superinterface_declares_instance_method(
    ctx: &dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
) -> bool {
    let Some(start) = ctx.class_id_by_name(class) else {
        return false;
    };
    let mut pending: Vec<ClassId> = Vec::new();
    let mut cur = Some(start);
    for _ in 0..64 {
        let Some(cid) = cur else {
            break;
        };
        pending.extend(ctx.class_interfaces(cid));
        cur = ctx.superclass_of(cid).filter(|&sup| sup != cid);
    }
    let mut seen: Vec<ClassId> = Vec::new();
    let mut budget = 1024usize;
    while let Some(iface) = pending.pop() {
        if seen.contains(&iface) {
            continue;
        }
        budget = match budget.checked_sub(1) {
            Some(b) => b,
            None => return false,
        };
        seen.push(iface);
        if lk_declared_method_flags(ctx, iface, name, desc)
            .is_some_and(|flags| flags & (LK_ACC_STATIC | LK_ACC_PRIVATE) == 0)
        {
            return true;
        }
        pending.extend(ctx.class_interfaces(iface));
    }
    false
}

/// The access flags of the method `class_id` ITSELF declares as `name desc`,
/// as the superinterface searches read them (interpreter round i5 wave 103,
/// lane L4; page `i89-L4-method-finder-per-class-and-per-handle-cost`,
/// "Wave 103" / "Wave 104"). `CRATONVM_MHN_RESOLVE_NAMED_SUPERINTERFACES`
/// (default off) asks the VM's method table directly
/// (`NativeClassAccess::declared_method_access_flags`: one read, no copy, no
/// string built). Wave 103 asked `declared_methods_named`, which still built
/// the match's four strings and decoded its `Exceptions` / `Signature`, and
/// lost the `--nojit` narrow / JDK rows (nothing to save on a 2-method table,
/// the same allocations paid). Off, the whole table is copied, as before.
/// Callers apply their own static / private mask, so both arms agree.
pub(crate) fn lk_declared_method_flags(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    desc: &str,
) -> Option<u16> {
    if crate::nbflags().mhn_resolve_named_superinterfaces {
        return ctx.declared_method_access_flags(class_id, name, desc);
    }
    ctx.declared_methods(class_id)
        .into_iter()
        .find(|m| m.name == name && m.descriptor == desc)
        .map(|m| m.access_flags)
}

/// Kill switch for the load-only finders (wave 17). `false` restores the
/// lookup-time `<clinit>` every finder below ran before.
pub(crate) const LOOKUP_FINDERS_LOAD_ONLY: bool = true;

/// Do the field finders judge the class `refc` IS (its mirror's id), as
/// HotSpot's `MemberName` resolution does, rather than whatever class of that
/// NAME the caller's loader finds (`i63-L4`)? `findGetter` / `findSetter`
/// took their class from a by-name load through the CALLING class's loader,
/// and the descriptor / static-ness / `final` checks
/// ([`lk_field_mismatch`]) and the absent-field check
/// ([`lookup_require_field`]) of every field finder looked the name up again:
/// a `refc` another loader defined was judged by a same-named class of the
/// caller's (a handle where HotSpot refuses; `I63FieldFinderJudgesRefc`).
/// `false` restores the by-name reading.
pub(crate) const LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR: bool = true;

/// The class a field finder judges: `refc`'s own id under
/// [`LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR`] (read before any GC point, as
/// the static finders read theirs), else -- or for a mirror that names no
/// class -- the by-name LOAD through the caller's loader.
pub(crate) fn lk_field_finder_refc(
    ctx: &mut dyn NativeContext,
    mirror_cid: Option<ClassId>,
    class: &str,
) -> Option<ClassId> {
    match mirror_cid {
        Some(cid) if LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR => Some(cid),
        _ => lookup_load_class(ctx, class).ok().flatten(),
    }
}

/// Kill switch for `MethodHandleNatives.resolve` alone (wave 17), the finder
/// the JDK's own `java.lang.invoke` boot goes through under `--jdk-only`.
pub(crate) const MHN_RESOLVE_LOAD_ONLY: bool = true;

/// Load `class` for a `Lookup` finder WITHOUT initializing it, answering its
/// id (`Ok(None)` only from a context that loads nothing, e.g. a test mock).
///
/// HotSpot's `find*` / `unreflect*` / `MethodHandleNatives.resolve` run no
/// `<clinit>`: a `DirectMethodHandle` initializes a static member's or a
/// constructor's declaring class at its first call (`mh_dispatch`'s static
/// method, static field and constructor arms here), and an instance member
/// needs none (its receiver exists). Initializing at lookup ran `C.<clinit>`
/// at `findVirtual(C.class, ..)` / `findConstructor` / `unreflect` time.
/// `Err` is the LOAD failure, the evidence rule of [`lookup_require_method`].
pub(crate) fn lookup_load_class(
    ctx: &mut dyn NativeContext,
    class: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    if !LOOKUP_FINDERS_LOAD_ONLY {
        return ctx.ensure_class_initialized(class).map(Some);
    }
    match ctx.load_class(class)? {
        Some(Value::Object(Some(mirror))) => {
            Ok(mirror_class_id(&*ctx, mirror).or_else(|| ctx.class_id_by_name(class)))
        }
        _ => Ok(ctx.class_id_by_name(class)),
    }
}

/// `Lookup.findGetter`/`findSetter` must raise `NoSuchFieldException` — a
/// *checked* exception — when the field is absent, for the same reason
/// [`lookup_require_method`] must raise `NoSuchMethodException`: handing back
/// a handle and failing at invoke time produces an `Error`, which sails
/// through the `catch (Exception)` of every version-probing library.
///
/// The old code deliberately never threw, because `resolve_field_index` walks
/// only the real class hierarchy and answers `None` for synthetic-stub classes
/// whose fields we do not model — so a bare `None` is not evidence of absence.
/// This keeps that escape hatch and adds the evidence the old code lacked:
/// only refuse when we could actually enumerate declared fields somewhere on
/// the chain (`saw_fields`) and the name was not among them. A class whose
/// whole chain enumerates empty is still admitted.
///
/// Takes the ClassId (not the mirror `ObjectRef`) because the only callers
/// have already loaded the class (`lookup_load_class`), which can move an unpinned
/// mirror out from under them.
pub(crate) fn lookup_require_field(
    ctx: &mut dyn NativeContext,
    class_id: Option<cratonvm_types::ClassId>,
    class: &str,
    name: &str,
    field_desc: &str,
    ref_kind: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // The by-name answer speaks for `class_id` only when the name finds that
    // very class (`LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR`): another loader's
    // same-named class proves nothing about `refc`.
    let by_name_is_refc = !LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR
        || class_id.is_none()
        || ctx.class_id_by_name(class) == class_id;
    if by_name_is_refc && ctx.resolve_field_index(class, name).is_some() {
        return Ok(());
    }
    let mut cid = match class_id {
        Some(id) => id,
        None => return Ok(()),
    };
    let mut saw_fields = false;
    loop {
        let fields = ctx.declared_fields(cid);
        if !fields.is_empty() {
            saw_fields = true;
            if fields.iter().any(|f| f.name == name) {
                return Ok(());
            }
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => break,
        }
    }
    if saw_fields {
        return Err(lk_no_such_field(ctx, class, name, field_desc, ref_kind));
    }
    Ok(())
}

pub(crate) fn lookup_find_static(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(ctx, args, 2, None, false, "invokeStatic")
        .map_err(|r| r.into_error(ctx))?;
    // Wave 81: another nest's private method.
    // Its refreshed copy of `args` when it loaded a nest host (a GC point).
    let refreshed = lk_enforce_private_nest_access(ctx, args, 2, None, "invokeStatic")?;
    let args: &[Value] = refreshed.as_deref().unwrap_or(args);
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let mt_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    if let Some(proxy) = lk_lambda_proxy_of_mirror(&*ctx, class_obj) {
        return lk_lambda_proxy_find(ctx, proxy, name_obj, mt_obj, "invokeStatic");
    }
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let desc = descriptor_from_method_type(ctx, mt_obj);
    let owner = mh_static_owner_of_mirror(&*ctx, Some(class_obj));
    // Read before the load below, a GC point for both references.
    let strict = mhn_strict_method_descriptor(&*ctx, mt_obj);
    let class_id = ctx.class_id_from_mirror(class_obj);
    // LOAD, never initialize: HotSpot's `findStatic` runs no `<clinit>`; the
    // handle's first call initializes the method's DECLARING class
    // (`DirectMethodHandle.ensureInitialized`, the static arm of
    // `mh_dispatch`). Initializing `refc` here ran `C.<clinit>` for a static
    // `C` inherits, at lookup time (wave 16; probe
    // `L3/InheritedStaticRoutes`, row `mh`).
    let loaded = ctx.load_class(&class).is_ok();
    // The mirror image of `findVirtual`'s check: `findStatic` on an INSTANCE
    // method is `IllegalAccessException: no such method`. A static method only
    // a superinterface declares is not inherited (JVMS 5.4.3.3), so it is
    // absent here (wave 24).
    lk_check_method(
        &*ctx,
        loaded,
        class_id,
        &class,
        &name,
        &desc,
        strict.as_deref(),
        "invokeStatic",
        true,
    )?;
    let mh = alloc_method_handle_with_receiver(
        ctx,
        &class,
        &name,
        &desc,
        MH_KIND_STATIC,
        None,
        class_id,
    )?;
    mh_record_static_owner(ctx, mh, owner);
    Ok(Some(Value::Object(Some(mh))))
}

pub(crate) fn lookup_find_constructor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(
        ctx,
        args,
        usize::MAX,
        Some("<init>"),
        false,
        "newInvokeSpecial",
    )
    .map_err(|r| r.into_error(ctx))?;
    // Wave 81: another nest's private constructor.
    // Its refreshed copy of `args` when it loaded a nest host (a GC point).
    let refreshed =
        lk_enforce_private_nest_access(ctx, args, usize::MAX, Some("<init>"), "newInvokeSpecial")?;
    let args: &[Value] = refreshed.as_deref().unwrap_or(args);
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let mt_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    // gen r4w3/rooting: `class_obj` is unrooted and `<clinit>` /
    // `alloc_method_handle` below are GC points; take its stable ClassId now
    // instead of reading the mirror again after them.
    let class_obj_cid = ctx.class_id_from_mirror(class_obj);
    let raw = descriptor_from_method_type(ctx, mt_obj);
    // Read before the load below, a GC point for `mt_obj`.
    let strict = mhn_strict_method_descriptor(&*ctx, mt_obj);
    // `Lookup.findConstructor`'s first statement, before any resolution:
    // "no constructor for array class: [I" (wave 24; it was
    // `NoSuchMethodException: [I.<init>()V`).
    if class.starts_with('[') {
        return Err(RuntimeError::NoSuchMethodException {
            message: format!(
                "no constructor for array class: {}",
                class.class_text(&*ctx)
            ),
        }
        .into());
    }
    // THE RETURN TYPE MUST BE `void`, and this is not pedantry about a value
    // nobody reads. `MethodHandles.Lookup.findConstructor` is specified to take
    // a type whose return is `void` -- the handle it gives back returns the
    // constructed class, but the type you ASK with does not. HotSpot answers
    // `NoSuchMethodException` for anything else. This code used to overwrite
    // whatever return type it was handed with `V` and carry on, so
    // `findConstructor(Target.class, methodType(Target.class, int.class))`
    // silently became the `(int)void` lookup instead of being refused.
    if !raw.ends_with(")V") {
        return Err(lk_no_such_method(
            &class,
            "<init>",
            &raw,
            "newInvokeSpecial",
        ));
    }
    let desc = raw;
    // Ensure class is loaded so constructor resolution works at dispatch time.
    // LOAD only: the handle's first call initializes it (`mh_dispatch`'s
    // constructor arm), as HotSpot's allocator handle does.
    let _ = lookup_load_class(ctx, &class);
    // AND THE CONSTRUCTOR HAS TO EXIST. There was no existence check here at
    // all: every other finder in this file grew one (see
    // `lookup_require_method`'s note on `catch (Exception)` version probes) and
    // this one was missed, so `findConstructor` for an arity the class does not
    // declare handed back a working-looking handle whose invocation would fail
    // later as an `Error`. An interface, a primitive and an array all declare
    // no constructor at all and take the same path.
    //
    // `method_exists` is the same evidence rule as everywhere else here: it is
    // consulted only when the class RESOLVED, so an unmodelled or synthetic
    // class still gets the permissive answer it always had.
    // An INTERFACE, a PRIMITIVE and an ARRAY declare no constructor at all --
    // ever, on any image. These three do not go through the "did the class
    // resolve" evidence rule below because they need no evidence: the answer is
    // a property of the KIND, not of what this VM happens to model. They are
    // also the two rows the evidence rule could not reach, since neither
    // `int` nor an unloaded `java/lang/Runnable` resolves by name here.
    let primitive = matches!(
        class.as_str(),
        "int" | "long" | "short" | "byte" | "char" | "boolean" | "float" | "double" | "void"
    );
    let interface = ctx
        .class_id_by_name(&class)
        .is_some_and(|cid| ctx.is_interface_class(cid));
    if primitive || interface {
        return Err(lk_no_such_method(
            &class,
            "<init>",
            &desc,
            "newInvokeSpecial",
        ));
    }
    // A CONSTRUCTOR IS NOT INHERITED (wave 24). `method_exists` walks the
    // superclass chain, so `findConstructor(Q.class, (int)void)` for a
    // `(int)` constructor only `Q`'s superclass declares came back as a
    // handle -- one whose invocation would run the superclass's constructor
    // on a `Q`. `lk_method_resolution` looks at the class's own constructors
    // only; `method_exists` stays the fallback when that cannot be proven.
    match lk_method_resolution(
        &*ctx,
        class_obj_cid,
        &class,
        "<init>",
        &desc,
        strict.as_deref(),
    ) {
        MhnVerdict::Absent => {
            return Err(lk_no_such_method(
                &class,
                "<init>",
                &desc,
                "newInvokeSpecial",
            ));
        }
        MhnVerdict::Found(_) => {}
        MhnVerdict::Unknown => {
            if ctx.class_id_by_name(&class).is_some() && !ctx.method_exists(&class, "<init>", &desc)
            {
                return Err(lk_no_such_method(
                    &class,
                    "<init>",
                    &desc,
                    "newInvokeSpecial",
                ));
            }
        }
    }
    let mh: ObjectRef = alloc_method_handle(ctx, &class, "<init>", &desc, MH_KIND_CONSTRUCTOR)?;
    // Stash the ALREADY-RESOLVED ClassId (from the caller's own Class
    // mirror, class_obj) in the otherwise-unused MH_BOUND slot. Two
    // classes minted under different ClassLoaders can share the same
    // binary name (e.g. Groovy re-parseClass-ing textually-similar
    // scripts) -- MH_KIND_CONSTRUCTOR dispatch re-resolving "class" (a
    // plain name string) at invoke time would collapse back to "one class
    // per name" and either construct the WRONG class or, since 2+ loaders
    // now register that name, silently fail (see dispatch_override in
    // execute_invoke_kind for the same class of bug on invokespecial).
    // Recording the identity here lets dispatch skip that re-resolution.
    if let Some(cid) = class_obj_cid {
        ctx.set_field(mh, MH_BOUND, Value::Int(cid.as_u32() as i32));
    }
    Ok(Some(Value::Object(Some(mh))))
}

pub(crate) fn lookup_find_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Before the pin/`ensure_class_initialized` block below: `args[0]` (the
    // Lookup) is NOT pinned across those calls, so the mode read has to happen
    // while the raw refs are still current.
    lk_enforce_find_access(ctx, args, 2, None, false, "invokeSpecial")
        .map_err(|r| r.into_error(ctx))?;
    // Wave 81: another nest's private method.
    // Its refreshed copy of `args` when it loaded a nest host (a GC point).
    let refreshed = lk_enforce_private_nest_access(ctx, args, 2, None, "invokeSpecial")?;
    let args: &[Value] = refreshed.as_deref().unwrap_or(args);
    // WP2.9 — Lookup.findSpecial(refc, name, type, specialCaller)
    //   args[0] = lookup (this)
    //   args[1] = refc (Class on which to find the method)
    //   args[2] = name (method name)
    //   args[3] = type (MethodType)
    //   args[4] = specialCaller (Class authorized to do invokespecial)
    // A null argument is a caller bug, `NullPointerException` on HotSpot as
    // for every other finder (`lookup_null_arg`); this one answered
    // `NoSuchMethodException` with an empty or partial member (wave 24).
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let mt_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    // A lambda proxy is final, so `specialCaller` restricts nothing: the
    // handle is the member's own, as `findVirtual`'s.
    if let Some(proxy) = lk_lambda_proxy_of_mirror(&*ctx, class_obj) {
        return lk_lambda_proxy_find(ctx, proxy, name_obj, mt_obj, "invokeSpecial");
    }
    // specialCaller (args[4]) — for spec correctness, we ensure it's loaded
    // so subsequent access checks work. The actual access check (specialCaller
    // must be the lookup class or have PRIVATE-mode access) is permissive
    // here: we trust JDK-side `Lookup.checkSpecial`. A stricter check would
    // require lookup-mode bookkeeping not yet wired through native-api.
    //
    // GC-safety: `ensure_class_initialized` below (both the conditional
    // caller-class load and the later target-class load) can trigger a
    // collection that relocates `class_obj`/`name_obj`/`mt_obj` (all
    // captured above, each read again afterward); pin them and re-read the
    // forwarded references before use.
    let class_obj_pin = ctx.pin_native_root(class_obj);
    let name_obj_pin = ctx.pin_native_root(name_obj);
    let mt_obj_pin = ctx.pin_native_root(mt_obj);
    // Read the caller's identity before `lookup_load_class` (a GC point): the
    // `specialCaller` mirror in `args[4]` is not pinned.
    let (caller_name, caller_id) = match args.get(4) {
        Some(Value::Object(Some(caller_obj))) => (
            resolve_class_name_robust(ctx, *caller_obj),
            ctx.class_id_from_mirror(*caller_obj),
        ),
        _ => (None, None),
    };
    if let Some(caller_name) = caller_name.as_deref() {
        // Ensure caller class is loaded so member-access verification has
        // both class hierarchies available downstream. Load only.
        let _ = lookup_load_class(ctx, caller_name);
    }
    let class_obj = ctx.read_native_pin(class_obj_pin, class_obj);
    let name_obj = ctx.read_native_pin(name_obj_pin, name_obj);
    let mt_obj = ctx.read_native_pin(mt_obj_pin, mt_obj);
    let class = resolve_class_name_robust(ctx, class_obj).unwrap_or_default();
    let refc_id = ctx.class_id_from_mirror(class_obj);
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let desc = descriptor_from_method_type(ctx, mt_obj);
    // Read before the load below, a GC point for both references.
    let strict = mhn_strict_method_descriptor(&*ctx, mt_obj);
    let class_id = ctx.class_id_from_mirror(class_obj);
    let loaded = lookup_load_class(ctx, &class).is_ok();
    ctx.unpin_native_roots(class_obj_pin);
    lk_check_method(
        &*ctx,
        loaded,
        class_id,
        &class,
        &name,
        &desc,
        strict.as_deref(),
        "invokeSpecial",
        false,
    )?;
    let dispatch_class = special_dispatch_class(&*ctx, &class, refc_id, caller_id, &name, &desc);
    // Typed `(specialCaller, ...)` by the mint (round 12 wave 4), which is the
    // receiver type interpreter round i1 wave 27 also stamps: the javadoc's
    // "a suitably restricted receiver type ... will be specialCaller or a
    // subtype"; HotSpot 25: `findSpecial(Base.class, "twice", ()int,
    // Sub.class).type()` is `(Sub)int`. `CRATONVM_MH_FIND_SPECIAL_JDK=0`
    // restores the pre-wave-4 `(refc, ...)` handle.
    // The dispatch class by id (wave 64, `CRATONVM_MH_SPECIAL_BY_OWNER_ID`),
    // read before the mint (a GC point; ids do not move).
    let special_owner = special_dispatch_owner(&*ctx, &dispatch_class, &class, refc_id, caller_id);
    let special_caller = special_caller_owner(&*ctx, caller_name.as_deref(), caller_id);
    let mh = alloc_method_handle_with_receiver(
        ctx,
        &dispatch_class,
        &name,
        &desc,
        MH_KIND_SPECIAL,
        special_receiver(caller_name.as_deref()),
        refc_id,
    )?;
    mh_record_special_owner(&*ctx, mh, special_owner);
    mh_record_special_caller(&*ctx, mh, special_caller);
    Ok(Some(Value::Object(Some(mh))))
}

/// The class an `invokespecial` handle made by `findSpecial` /
/// `unreflectSpecial` dispatches on.
///
/// JVMS §6.5 `invokespecial` (ACC_SUPER) and the JDK's
/// `Lookup.getDirectMethodCommon`: when the resolved class `refc` is a proper
/// superclass of `specialCaller` ABOVE its direct superclass (and neither is an
/// interface), the search restarts at `specialCaller`'s direct superclass, so
/// `findSpecial(Grand, m, Sub)` runs `Mid.m` when `Mid` overrides it -- what
/// `super.m()` in `Sub` runs. Dispatching on `refc` itself ran `Grand.m`.
/// Anything else (no caller, an interface on either side, an unrelated caller,
/// `refc` already the direct superclass or the caller itself) keeps `refc`, and
/// so does a method `refc` declares `private`: private methods are not
/// inherited, so the JDK's restarted search finds that same declaration.
pub(crate) fn special_dispatch_class(
    ctx: &dyn NativeContext,
    refc: &str,
    refc_id: Option<ClassId>,
    caller_id: Option<ClassId>,
    name: &str,
    desc: &str,
) -> String {
    let (Some(r), Some(c)) = (refc_id, caller_id) else {
        return refc.to_string();
    };
    if !find_special_jdk_enabled() {
        return refc.to_string();
    }
    if r == c || ctx.is_interface_class(r) || ctx.is_interface_class(c) {
        return refc.to_string();
    }
    let private_in_refc = ctx
        .declared_methods(r)
        .iter()
        .any(|m| m.name == name && m.descriptor == desc && m.access_flags & LK_ACC_PRIVATE != 0);
    if private_in_refc {
        return refc.to_string();
    }
    let Some(sup) = ctx.superclass_of(c) else {
        return refc.to_string();
    };
    if sup == r || !ctx.is_subclass(c, r) {
        return refc.to_string();
    }
    ctx.class_name_of_id(sup)
        .unwrap_or_else(|| refc.to_string())
}

/// The receiver type of a `findSpecial` / `unreflectSpecial` handle: the
/// `specialCaller` (see [`alloc_method_handle_with_receiver`]), or `None` (the
/// resolved class) with the kill switch off.
pub(crate) fn special_receiver(caller_name: Option<&str>) -> Option<&str> {
    caller_name.filter(|_| find_special_jdk_enabled())
}

/// `CRATONVM_MH_FIND_SPECIAL_JDK` -- default ON (round 12 wave 4, lane
/// hunter2); `0`/`false`/`off`/`no` restores the pre-wave-4 `findSpecial` /
/// `unreflectSpecial` handle, typed `(refc, ...)` and dispatching on `refc`
/// itself. Read per lookup (a cold path), in both modes: the JDK behaviour is
/// the same in `--compatible` and `--jdk-only`.
pub(crate) fn find_special_jdk_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_FIND_SPECIAL_JDK")
}

/// Create a NoSuchMethodException error.
/// `java.lang.NullPointerException` for a null argument to a `Lookup.find*`.
///
/// Every finder used to map a null `Class` / name / `MethodType` onto its own
/// "absent member" exception -- `NoSuchMethodException` or
/// `NoSuchFieldException`. Those are the answers for a member that is not
/// there, and a null argument is not a member that is not there: it is a caller
/// bug, and HotSpot says so with an NPE before it looks anything up.
///
/// The direction is what makes it worth fixing. Both wrong answers are CHECKED
/// exceptions that version-probing code catches ON PURPOSE -- see
/// `lookup_require_method`'s note on `catch (Exception)`. So a null slipped
/// past the probe's guard and was reported as "this JDK does not have that
/// method", a wrong conclusion the caller then acts on, instead of a stack
/// trace at the line with the bug.
pub(crate) fn lookup_null_arg(what: &str) -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
        cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some(format!("Lookup.find*: {what} must not be null")),
        },
    ))
}

/// `java.lang.IllegalAccessException` for a member that EXISTS but whose
/// static-ness, finality or kind is not what the finder asked for.
///
/// `MethodHandles.Lookup`: `findStatic` on an instance method, `findVirtual` on
/// a static one, `findGetter` on a static field, `findStaticGetter` on an
/// instance field and `findSetter` on a `final` field are all
/// `IllegalAccessException` -- NOT `NoSuchMethodException`. The member is
/// found; the ACCESS is what is refused, and a caller that distinguishes the
/// two learns different things from them.
pub(crate) fn lookup_access_error(what: String) -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
        cratonvm_types::error::RuntimeError::IllegalAccessException { message: what },
    ))
}

// ---------------------------------------------------------------------------
// HotSpot's `Lookup.find*` refusals (interpreter round i1 wave 24, lane L4)
// ---------------------------------------------------------------------------
//
// These natives answer `Lookup.find*` in BOTH modes, so the JDK's
// `MemberName.Factory.resolveOrFail` never runs and its refusals had to be
// rebuilt here. They were, but with CratonVM's own texts
// (`NoSuchMethodException: T.nope()V`, `IllegalAccessException: T.stat()V:
// expected a non-static method`) and with two resolution gaps: a constructor
// only a SUPERCLASS declares was found (`method_exists` walks superclasses,
// and constructors are not inherited), and a default method reached through a
// class was refused (`method_exists` does not walk superinterfaces).
//
// HotSpot builds every refusal in `MemberName.makeAccessException`:
// `<what>: <MemberName.toString()>`, where the member is
// `Owner.name(ParamSimpleNames)Ret/refKind` for a method or constructor and
// `Owner.name/TypeName/refKind` for a field, and a refusal raised by
// `Lookup.checkField` / `checkAccess` (static-ness of a field, a final field's
// setter) appends `, from <lookupClass> (<module>)` or `, from public Lookup`.
// A method of the other static-ness fails RESOLUTION
// (`IncompatibleClassChangeError`), so it reads `no such method` but is an
// `IllegalAccessException`. Measured on HotSpot 25.0.3 by
// `tools/probes/interp/L4/L4W23LookupResolveErrors.java` and
// `L4W24LookupFinderMessages.java`.
//
// Resolution reuses `native_mhn_resolve`'s wave-23 searches
// (`mhn_method_verdict`, `mhn_field_resolve`); whenever they cannot prove an
// answer (`Unknown`: a stub on the path, a hidden class in the type, nothing
// enumerable) the finders keep their older checks, now with the same texts.

/// `MemberName.toString()` of the METHOD or CONSTRUCTOR a finder asked for:
/// the owner's `Class.getName()`, the name, the requested
/// `MethodType.toString()` and the reference kind.
pub(crate) fn lk_method_member_display(class: &str, name: &str, desc: &str, ref_kind: &str) -> String {
    let ty = method_type_display(desc).unwrap_or_else(|| desc.to_string());
    format!("{}.{name}{ty}/{ref_kind}", class.hidden_dotted())
}

/// `MemberName.toString()` of a FIELD: `Owner.name/TypeName/refKind`, the
/// type as `Class.getName()` (`int`, `java.lang.String`, `[Ljava.lang.String;`).
pub(crate) fn lk_field_member_display(class: &str, name: &str, field_desc: &str, ref_kind: &str) -> String {
    format!(
        "{}.{name}/{}/{ref_kind}",
        class.hidden_dotted(),
        mh_class_get_name(field_desc)
    )
}

/// `NoSuchMethodException: no such method|constructor: <member>`.
pub(crate) fn lk_no_such_method(class: &str, name: &str, desc: &str, ref_kind: &str) -> MethodCallFailed {
    let what = if name == "<init>" {
        "no such constructor"
    } else {
        "no such method"
    };
    RuntimeError::NoSuchMethodException {
        message: format!(
            "{what}: {}",
            lk_method_member_display(class, name, desc, ref_kind)
        ),
    }
    .into()
}

/// A method of the other static-ness: resolution raised
/// `IncompatibleClassChangeError`, which `makeAccessException` reports as
/// `IllegalAccessException: no such method: <member>`.
pub(crate) fn lk_method_static_mismatch(
    class: &str,
    name: &str,
    desc: &str,
    ref_kind: &str,
) -> MethodCallFailed {
    lookup_access_error(format!(
        "no such method: {}",
        lk_method_member_display(class, name, desc, ref_kind)
    ))
}

/// `NoSuchFieldException: no such field: <member>`, caused (as HotSpot's
/// `MemberName.makeAccessException` has it) by the resolution's
/// `NoSuchFieldError: Class <C> does not have member field '<T> <name>'`
/// (interpreter round i1 wave 49, lane L4): `ConstantBootstraps` and
/// `MethodHandleNatives.mapLookupExceptionToError` surface that cause
/// (`L4W48GetStaticFinal` row `missing`). Message-only when the throwables
/// cannot be built.
pub(crate) fn lk_no_such_field(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    field_desc: &str,
    ref_kind: &str,
) -> MethodCallFailed {
    let message = format!(
        "no such field: {}",
        lk_field_member_display(class, name, field_desc, ref_kind)
    );
    lk_no_such_field_with_cause(ctx, &message, class, name, field_desc).unwrap_or_else(|| {
        RuntimeError::NoSuchFieldException {
            field_name: message,
        }
        .into()
    })
}

/// A field descriptor as the Java language names the type (`int[]`,
/// `java.lang.String[][]`), the form HotSpot's resolution errors print.
pub(crate) fn java_type_display(field_desc: &str) -> String {
    let base = field_desc.trim_start_matches('[');
    let dims = field_desc.len() - base.len();
    let mut out = descriptor_to_class_name(base).hidden_dotted();
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

/// The throwing half of [`lk_no_such_field`]: the exception with its cause
/// attached, or `None` when either could not be constructed.
#[cold]
#[inline(never)]
pub(crate) fn lk_no_such_field_with_cause(
    ctx: &mut dyn NativeContext,
    message: &str,
    class: &str,
    name: &str,
    field_desc: &str,
) -> Option<MethodCallFailed> {
    let cause_text = format!(
        "Class {} does not have member field '{} {name}'",
        class.class_text(&*ctx),
        java_type_display(field_desc)
    );
    let cause_msg = ctx.create_string_uninterned(&cause_text);
    let Ok(Some(Value::Object(Some(cause)))) = ctx.new_object_initialized(
        "java/lang/NoSuchFieldError",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(cause_msg))],
    ) else {
        return None;
    };
    // The cause stays pinned across the outer object's allocation and is
    // attached with `initCause` afterwards (`array_type_unsupported`'s idiom).
    let cause_pin = ctx.pin_native_root(cause);
    let msg = ctx.create_string_uninterned(message);
    let outer = ctx.new_object_initialized(
        "java/lang/NoSuchFieldException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
    );
    let Ok(Some(Value::Object(Some(outer)))) = outer else {
        ctx.unpin_native_roots(cause_pin);
        return None;
    };
    let outer_pin = ctx.pin_native_root(outer);
    let cause = ctx.read_native_pin(cause_pin, cause);
    let _ = ctx.invoke_virtual(
        outer,
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        &[Value::Object(Some(cause))],
    );
    let outer = ctx.read_native_pin(outer_pin, outer);
    ctx.unpin_native_roots(cause_pin);
    Some(MethodCallFailed::ExceptionThrown(outer))
}

/// Who asked, for the `, from ...` of a `checkField` / `checkAccess` refusal.
/// Captured at the top of a finder, while `args` is still current; rendered
/// by [`lk_from_suffix`] only on the refusal path.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LkFrom {
    /// `publicLookup()` (the only `UNCONDITIONAL`-only Lookup we mint).
    Public,
    /// A Lookup whose `lookupClass` is this class.
    Class(ClassId),
    /// The JDK's `IMPL_LOOKUP` (`allowedModes == TRUSTED`, -1), which
    /// `Lookup.checkAccess` waves through before its final-field rule
    /// (`checkField`'s static-ness rule still applies).
    Trusted(Option<ClassId>),
    /// Unreadable: the suffix is left out rather than guessed.
    Unknown,
}

impl LkFrom {
    pub(crate) fn is_trusted(self) -> bool {
        matches!(self, LkFrom::Trusted(_))
    }
}

pub(crate) fn lk_from_of(ctx: &dyn NativeContext, args: &[Value]) -> LkFrom {
    let Some(Value::Object(Some(this))) = args.first() else {
        return LkFrom::Unknown;
    };
    let modes = lk_read_allowed_modes_opt(ctx, *this);
    if modes == Some(LK_MODE_UNCONDITIONAL) {
        return LkFrom::Public;
    }
    let class_id = match ctx.get_field(*this, 0) {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    };
    if modes == Some(-1) {
        return LkFrom::Trusted(class_id);
    }
    class_id.map_or(LkFrom::Unknown, LkFrom::Class)
}

/// `MemberName.makeAccessException(String, Object)`'s tail:
/// `, from public Lookup`, or `, from class X (unnamed module @1b6d3586)`.
/// Runs `Class.getModule()` and `Module.toString()` (Java code, a GC point),
/// so a caller renders it after every other read. The JDK's
/// `, previous lookup ...` clause is not rebuilt: our Lookups carry a null
/// `prevLookupClass` (see `Lookup.in`'s native).
pub(crate) fn lk_from_suffix(ctx: &mut dyn NativeContext, from: LkFrom) -> String {
    let cid = match from {
        LkFrom::Public => return ", from public Lookup".to_string(),
        LkFrom::Unknown | LkFrom::Trusted(None) => return String::new(),
        LkFrom::Class(cid) | LkFrom::Trusted(Some(cid)) => cid,
    };
    let Some(name) = ctx.class_name_of_id(cid) else {
        return String::new();
    };
    let shown = mh_class_to_string(&*ctx, &class_name_to_descriptor(&name));
    let mirror = ctx.get_class_mirror(cid);
    let module = match ctx.invoke_virtual(mirror, "getModule", "()Ljava/lang/Module;", &[]) {
        Ok(Some(Value::Object(Some(m)))) => Some(m),
        _ => None,
    };
    let module_text = match module {
        Some(m) => match ctx.invoke_virtual(m, "toString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
            _ => None,
        },
        None => None,
    };
    match module_text {
        Some(text) => format!(", from {shown} ({text})"),
        None => format!(", from {shown}"),
    }
}

/// JVMS 5.4.3.3 / 5.4.3.4 resolution of the method (or constructor) a finder
/// asks for, as far as the declared members PROVE it: [`mhn_method_verdict`]
/// under the rules `native_mhn_resolve` applies to it. `Unknown` whenever the
/// descriptor cannot be rebuilt exactly, a compatibility stub is on the
/// class's chain, or (for a method) the native registry answers for a member
/// no class file declares.
pub(crate) fn lk_method_resolution(
    ctx: &dyn NativeContext,
    class_id: Option<ClassId>,
    class: &str,
    name: &str,
    desc: &str,
    strict: Option<&str>,
) -> MhnVerdict {
    let (Some(cid), Some(strict)) = (class_id, strict) else {
        return MhnVerdict::Unknown;
    };
    if strict != desc || mhn_class_chain_has_stub(ctx, class) {
        return MhnVerdict::Unknown;
    }
    match mhn_method_verdict(ctx, cid, name, strict) {
        // `method_exists` also answers from the native registry. A
        // constructor is exempt: it walks superclasses, and a superclass's
        // `<init>` is exactly what must NOT count (wave-23 rule, same as
        // `native_mhn_resolve`'s).
        MhnVerdict::Absent if name != "<init>" && ctx.method_exists(class, name, desc) => {
            MhnVerdict::Unknown
        }
        verdict => verdict,
    }
}

/// Which field finder is asking.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LkFieldFinder {
    Getter,
    Setter,
    StaticGetter,
    StaticSetter,
    VarHandle,
    StaticVarHandle,
}

impl LkFieldFinder {
    pub(crate) fn wants_static(self) -> bool {
        matches!(
            self,
            LkFieldFinder::StaticGetter
                | LkFieldFinder::StaticSetter
                | LkFieldFinder::StaticVarHandle
        )
    }

    pub(crate) fn is_setter(self) -> bool {
        matches!(self, LkFieldFinder::Setter | LkFieldFinder::StaticSetter)
    }

    /// The reference kind of a field of static-ness `is_static` under this
    /// finder. `find(Static)VarHandle` resolves as a getter.
    pub(crate) fn ref_kind(self, is_static: bool) -> &'static str {
        match (self.is_setter(), is_static) {
            (false, false) => "getField",
            (false, true) => "getStatic",
            (true, false) => "putField",
            (true, true) => "putStatic",
        }
    }

    /// The reference kind the finder REQUESTS (a missing field keeps it).
    pub(crate) fn requested_ref_kind(self) -> &'static str {
        self.ref_kind(self.wants_static())
    }
}

/// HotSpot's answer for a field finder, when it can be proven: resolution by
/// name AND type ([`mhn_field_resolve`]; absent is `NoSuchFieldException`),
/// then `Lookup.checkField` (the other static-ness is `IllegalAccessException:
/// expected a (non-)static field`, shown with the FOUND field's declaring class
/// and reference kind) and `checkAccess`'s final-field rule for a setter
/// (`unexpected set of a final field`).
///
/// `Ok(true)`: proven acceptable, the caller's older checks are skipped.
/// `Ok(false)`: not provable, the caller's older checks decide.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lk_field_resolution(
    ctx: &mut dyn NativeContext,
    finder: LkFieldFinder,
    class_id: Option<ClassId>,
    class: &str,
    name: &str,
    field_desc: &str,
    strict: Option<&str>,
    from: LkFrom,
) -> Result<bool, MethodCallFailed> {
    let (Some(cid), Some(strict)) = (class_id, strict) else {
        return Ok(false);
    };
    if strict != field_desc {
        return Ok(false);
    }
    let (flags, declaring) = match mhn_field_resolve(&*ctx, cid, name, strict) {
        None => return Ok(false),
        Some(None) => {
            return Err(lk_no_such_field(
                ctx,
                class,
                name,
                field_desc,
                finder.requested_ref_kind(),
            ));
        }
        Some(Some(found)) => found,
    };
    let is_static = flags & MHN_ACC_STATIC != 0;
    let owner = ctx
        .class_name_of_id(declaring)
        .unwrap_or_else(|| class.to_string());
    let shown = lk_field_member_display(&owner, name, field_desc, finder.ref_kind(is_static));
    if is_static != finder.wants_static() {
        let what = if finder.wants_static() {
            "expected a static field"
        } else {
            "expected a non-static field"
        };
        let from = lk_from_suffix(ctx, from);
        return Err(lookup_access_error(format!("{what}: {shown}{from}")));
    }
    if finder.is_setter() && flags & LK_ACC_FINAL != 0 && !from.is_trusted() {
        let from = lk_from_suffix(ctx, from);
        return Err(lookup_access_error(format!(
            "unexpected set of a final field: {shown}{from}"
        )));
    }
    // `checkAccess` -> `VerifyAccess.isMemberAccessible`, PROTECTED: a lookup
    // class of another run-time package that is not a subclass of the
    // declaring class is refused (`accessFailedMessage`: "member is
    // protected"), whatever its lookup modes. `lk_enforce_find_access` lets
    // every full-power lookup through before any member check, so this was
    // admitted (interpreter round i1 wave 45, lane L4; `L4W44ProtectedReceiver`
    // row `unrelated-getter`, measured on HotSpot 25).
    if lk_protected_member_refused(&*ctx, from, declaring, flags) {
        let from = lk_from_suffix(ctx, from);
        return Err(lookup_access_error(format!(
            "member is protected: {shown}{from}"
        )));
    }
    Ok(true)
}

/// Does `VerifyAccess.isMemberAccessible` refuse a PROTECTED member of
/// `declaring` (access `flags`) to the lookup class of `from`? Only a
/// positive reading: a lookup class in another run-time package (package
/// name and loader) that is neither `declaring` nor a subclass of it. A
/// trusted, public or unreadable lookup is never refused here (the public
/// lookup's own rule is `lk_enforce_find_access`'s).
pub(crate) fn lk_protected_member_refused(
    ctx: &dyn NativeContext,
    from: LkFrom,
    declaring: ClassId,
    flags: u16,
) -> bool {
    const ACC_PROTECTED: u16 = 0x0004;
    let LkFrom::Class(lookup) = from else {
        return false;
    };
    if flags & ACC_PROTECTED == 0 || lookup == declaring || ctx.is_subclass(lookup, declaring) {
        return false;
    }
    let (Some(lookup_name), Some(declaring_name)) = (
        ctx.class_name_of_id(lookup),
        ctx.class_name_of_id(declaring),
    ) else {
        return false;
    };
    let package = |n: &str| {
        n.rsplit_once('/')
            .map_or(String::new(), |(p, _)| p.to_string())
    };
    let same_package = package(&lookup_name) == package(&declaring_name)
        && ctx.loader_id_of_class(lookup) == ctx.loader_id_of_class(declaring);
    !same_package
}

pub(crate) const LK_ACC_STATIC: u16 = 0x0008;
pub(crate) const LK_ACC_FINAL: u16 = 0x0010;

/// Is `class.name desc` declared STATIC?
///
/// `None` means UNKNOWN -- the class did not resolve, or nothing on its
/// superclass chain declared that exact (name, descriptor). **Every caller must
/// treat `None` as an ACCEPT.** This VM substitutes and synthesises JDK classes
/// whose declared members it does not always model, so "could not see it" is
/// not evidence of anything, and refusing on it would turn a modelling gap into
/// a refusal of working code. Same rule `lookup_require_field` already follows,
/// and the same reason.
pub(crate) fn lk_method_is_static(
    ctx: &dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
) -> Option<bool> {
    let mut cid = ctx.class_id_by_name(class)?;
    for _ in 0..64 {
        for m in ctx.declared_methods(cid) {
            if m.name == name && m.descriptor == desc {
                return Some(m.access_flags & LK_ACC_STATIC != 0);
            }
        }
        cid = ctx.superclass_of(cid)?;
    }
    None
}

/// The declared field `class.name`, searched up the superclass chain from
/// `class_id` (the finder's `refc`) under
/// [`LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR`], else from the class `class`
/// names.
///
/// `None` is UNKNOWN and an ACCEPT, for the reason on [`lk_method_is_static`].
pub(crate) fn lk_declared_field(
    ctx: &dyn NativeContext,
    class_id: Option<ClassId>,
    class: &str,
    name: &str,
) -> Option<cratonvm_native_api::FieldMetadata> {
    let start = class_id.filter(|_| LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR);
    let mut cid = start.or_else(|| ctx.class_id_by_name(class))?;
    for _ in 0..64 {
        for f in ctx.declared_fields(cid) {
            if f.name == name {
                return Some(f);
            }
        }
        cid = ctx.superclass_of(cid)?;
    }
    None
}

/// The `IllegalAccessException` / `NoSuchFieldException` a field finder owes
/// when the field EXISTS but does not match what was asked for, or `None`.
///
/// Three checks, all of them positive readings only -- an unresolvable class or
/// an unmodelled field yields `None` and the finder proceeds exactly as it did
/// before ([`lk_declared_field`]).
///
///  * the DESCRIPTOR must match. `findGetter(C, "f", String.class)` on an
///    `int f` is `NoSuchFieldException` on HotSpot, because a field is
///    identified by name AND type; we answered a handle whose invocation would
///    later read an int as a String.
///  * STATIC-ness must match the finder. `findGetter` on a static field and
///    `findStaticGetter` on an instance field are both
///    `IllegalAccessException`.
///  * a `final` field has no SETTER. `findSetter` on one is
///    `IllegalAccessException`; we handed back a handle that would have written
///    it.
///
/// The fallback of [`lk_field_resolution`] (wave 24), which answers first
/// whenever it can prove resolution; the texts are HotSpot's either way.
pub(crate) fn lk_field_mismatch(
    ctx: &mut dyn NativeContext,
    finder: LkFieldFinder,
    class_id: Option<ClassId>,
    class: &str,
    name: &str,
    want_desc: &str,
    from: LkFrom,
) -> Option<cratonvm_types::error::MethodCallFailed> {
    let f = lk_declared_field(&*ctx, class_id, class, name)?;
    if f.descriptor != want_desc {
        return Some(lk_no_such_field(
            ctx,
            class,
            name,
            want_desc,
            finder.requested_ref_kind(),
        ));
    }
    let is_static = f.is_static || (f.access_flags & LK_ACC_STATIC != 0);
    let owner = ctx
        .class_name_of_id(f.declaring_class_id)
        .unwrap_or_else(|| class.to_string());
    let shown = lk_field_member_display(&owner, name, want_desc, finder.ref_kind(is_static));
    if is_static != finder.wants_static() {
        let what = if finder.wants_static() {
            "expected a static field"
        } else {
            "expected a non-static field"
        };
        let from = lk_from_suffix(ctx, from);
        return Some(lookup_access_error(format!("{what}: {shown}{from}")));
    }
    if finder.is_setter() && f.access_flags & LK_ACC_FINAL != 0 && !from.is_trusted() {
        let from = lk_from_suffix(ctx, from);
        return Some(lookup_access_error(format!(
            "unexpected set of a final field: {shown}{from}"
        )));
    }
    None
}

pub(crate) fn no_such_method_error(
    class: &str,
    method: &str,
    desc: &str,
) -> cratonvm_types::error::MethodCallFailed {
    // A REAL `java.lang.NoSuchMethodException`, not a `VmError::Internal` whose
    // message merely spells one. `Lookup.find*` is the standard way libraries
    // version-probe an API, and they guard it with `catch (Exception)` — see
    // the note on `lookup_find_virtual`.
    cratonvm_types::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
        cratonvm_types::error::RuntimeError::NoSuchMethodException {
            message: format!("{class}.{method}{desc}"),
        },
    ))
}

/// Create a NoSuchFieldException error.
pub(crate) fn no_such_field_error(class: &str, field: &str) -> cratonvm_types::error::MethodCallFailed {
    // A REAL `java.lang.NoSuchFieldException` — see `no_such_method_error`.
    cratonvm_types::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
        cratonvm_types::error::RuntimeError::NoSuchFieldException {
            field_name: format!("{class}.{field}"),
        },
    ))
}

pub(crate) fn lookup_find_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(ctx, args, 2, None, true, "getField").map_err(|r| r.into_error(ctx))?;
    lk_enforce_lookup_class_field_access(ctx, args, "getField")?;
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let type_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    if let Some(proxy) = lk_lambda_proxy_of_mirror(&*ctx, class_obj) {
        return lk_lambda_proxy_field_find(ctx, args, proxy, name_obj, type_obj, false);
    }
    // Stage 2's owner (wave 63, `CRATONVM_MH_OWNER_ID_AUTHORITY`): `refc`'s id,
    // read while `class_obj` is current.
    let field_owner = mh_field_owner_of_mirror(&*ctx, Some(class_obj));
    // `refc` itself (i63-L4), read while `class_obj` is current.
    let mirror_cid = ctx.class_id_from_mirror(class_obj);
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    // Read before the load below, a GC point for every reference in `args`.
    let strict = mhn_strict_type_descriptor(&*ctx, type_obj);
    let from = lk_from_of(&*ctx, args);
    // Instance field: LOAD only, a receiver initializes the class.
    let target_cid = lk_field_finder_refc(ctx, mirror_cid, &class);
    let finder = LkFieldFinder::Getter;
    if !lk_field_resolution(
        ctx,
        finder,
        target_cid,
        &class,
        &name,
        &field_desc,
        strict.as_deref(),
        from,
    )? {
        if let Some(e) =
            lk_field_mismatch(ctx, finder, target_cid, &class, &name, &field_desc, from)
        {
            return Err(e);
        }
        // HotSpot raises `NoSuchFieldException` here. `lookup_require_field`
        // only does so when it could enumerate declared fields and the name
        // was not among them, which preserves the stub-class escape hatch
        // this call site used to buy with a blanket `let _ = ...`. (H2's
        // version probe needs `findGetter` to SUCCEED as its fallback — it
        // does, the field is there.)
        lookup_require_field(
            ctx,
            target_cid,
            &class,
            &name,
            &field_desc,
            finder.requested_ref_kind(),
        )?;
    }
    let desc = format!("(L{class};){field_desc}");
    let restricted = lk_protected_receiver(&*ctx, from, target_cid, LkMember::Field(&name));
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_GETTER)?;
    mh_record_instance_owner(&*ctx, mh, field_owner);
    mh_record_minted_field_slot(&*ctx, mh, field_owner, &name);
    if let Some(receiver) = restricted {
        return lk_restrict_receiver_type(ctx, mh, &format!("(L{receiver};){field_desc}"));
    }
    Ok(Some(Value::Object(Some(mh))))
}

/// The member [`lk_protected_receiver`] asks about: a field by name, or a
/// method by name and descriptor (without the receiver).
#[derive(Clone, Copy)]
pub(crate) enum LkMember<'a> {
    Field(&'a str),
    Method(&'a str, &'a str),
}

/// JDK 25 `Lookup.restrictProtectedReceiver` + `restrictReceiver`: a
/// PROTECTED instance member of a class in another run-time package, reached
/// through a lookup of its subclass, gets a handle whose receiver type is the
/// LOOKUP class (`findGetter(ByteArrayOutputStream.class, "count", int.class)`
/// from a subclass `Sub` is `(Sub)int`; measured, HotSpot 25,
/// `L4W43ObjectMethodsJdkRoute` row `protected-type`). The internal name of
/// the lookup class, or `None` when no restriction applies or the model
/// cannot tell (a trusted, public or unreadable lookup). Only the handle's
/// `type` changes: dispatch still reads the member through `MH_CLASS`.
/// Interpreter round i1 wave 43, lane L4 (fields, `findGetter`); wave 44 took
/// it to every finder `getDirectMethodCommon` / `getDirectFieldCommon` apply
/// it in (`findSetter`, `findVirtual`, `unreflect`, `unreflectGetter`,
/// `unreflectSetter`), and added the JDK's two exemptions:
///
///  * `restrictReceiver`'s "already narrow": `refc` the lookup class or a
///    subclass of it keeps `refc` (`findGetter(SubSub.class, ..)` from `Sub`
///    is `(SubSub)int`);
///  * `isArrayClone`: an array's inherited `clone` keeps the array type.
///
/// `L4W44ProtectedReceiver`.
pub(crate) fn lk_protected_receiver(
    ctx: &dyn NativeContext,
    from: LkFrom,
    refc: Option<ClassId>,
    member: LkMember<'_>,
) -> Option<String> {
    lk_protected_receiver_of(ctx, from, refc, member, true).map(|(name, _)| name)
}

/// [`lk_protected_receiver`] for a `VarHandle` (`findVarHandle`,
/// `unreflectVarHandle`): JDK 25 `getFieldVarHandleCommon` sets `refc =
/// lookupClass()` whenever `restrictProtectedReceiver` answers, with no
/// "already narrow" exemption (`findVarHandle(SubSub.class, "count", ..)` from
/// `Sub` has the coordinate `Sub`; measured, `L4W45ProtectedVarHandle`). The
/// lookup class's name and id. Interpreter round i1 wave 45, lane L4.
pub(crate) fn lk_protected_var_handle_receiver(
    ctx: &dyn NativeContext,
    from: LkFrom,
    refc: Option<ClassId>,
    field_name: &str,
) -> Option<(String, ClassId)> {
    lk_protected_receiver_of(ctx, from, refc, LkMember::Field(field_name), false)
}

/// The class a `VarHandle` of instance field `field_name` (resolved in `refc`
/// at slot `field_index`) is minted against when its receiver is restricted
/// ([`lk_protected_var_handle_receiver`]): the lookup class, whose name
/// becomes `VarHandleMeta::class_name` (the coordinate, and the receiver
/// check's cast target) and whose id `VarHandleMeta::class_id` (the thin
/// helpers' exact-class compare; any other receiver declines to the native,
/// which admits a subclass and refuses the declaring class's own instance).
///
/// Applied whatever the lookup class declares: the by-name readers
/// (`vh_instance_field_by_name`, `toMethodHandle`'s accessor) look the name
/// up in the field's holder (`VarHandleMeta::holder_class_id`, `refc`), not
/// in the coordinate, so a lookup class that declares a HIDING field of the
/// same name is restricted as HotSpot restricts it (its handle holds the
/// field's offset). Until wave 46 such a hiding field kept the unrestricted
/// handle (`L4W46VarHandleHidingField`).
pub(crate) fn vh_protected_coordinate(
    ctx: &dyn NativeContext,
    from: LkFrom,
    refc: ClassId,
    field_name: &str,
) -> Option<(String, ClassId)> {
    lk_protected_var_handle_receiver(ctx, from, Some(refc), field_name)
}

pub(crate) fn lk_protected_receiver_of(
    ctx: &dyn NativeContext,
    from: LkFrom,
    refc: Option<ClassId>,
    member: LkMember<'_>,
    narrow_exempt: bool,
) -> Option<(String, ClassId)> {
    const ACC_PROTECTED: u16 = 0x0004;
    const ACC_STATIC: u16 = 0x0008;
    let LkFrom::Class(lookup) = from else {
        return None;
    };
    let refc = refc?;
    // "already narrow": the handle's receiver is `refc`, which the lookup
    // class is assignable from. `refc` the lookup class itself restricts to
    // the same class, so a VarHandle skips only the subclass half.
    if refc == lookup || (narrow_exempt && ctx.is_subclass(refc, lookup)) {
        return None;
    }
    let refc_name = ctx.class_name_of_id(refc)?;
    if let LkMember::Method(name, _) = member {
        // `isArrayClone`; and an interface's `refc` never resolves one of
        // `Object`'s protected methods.
        if (name == "clone" && refc_name.starts_with('[')) || ctx.is_interface_class(refc) {
            return None;
        }
    }
    // The declaring class: the first on `refc`'s superclass chain that
    // declares the member (superinterfaces declare only static fields and
    // public or private methods).
    let mut cid = refc;
    let mut depth = 0usize;
    let (declaring, flags) = loop {
        let found = match member {
            LkMember::Field(name) => ctx
                .declared_fields(cid)
                .into_iter()
                .find(|f| f.name == name)
                .map(|f| f.access_flags),
            LkMember::Method(name, desc) => ctx
                .declared_methods(cid)
                .into_iter()
                .find(|m| m.name == name && m.descriptor == desc)
                .map(|m| m.access_flags),
        };
        if let Some(flags) = found {
            break (cid, flags);
        }
        depth += 1;
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid && depth < 256 => cid = parent,
            _ => return None,
        }
    };
    if flags & ACC_PROTECTED == 0 || flags & ACC_STATIC != 0 || declaring == lookup {
        return None;
    }
    let lookup_name = ctx.class_name_of_id(lookup)?;
    let declaring_name = ctx.class_name_of_id(declaring)?;
    let package = |n: &str| {
        n.rsplit_once('/')
            .map_or(String::new(), |(p, _)| p.to_string())
    };
    if package(&lookup_name) == package(&declaring_name)
        && ctx.loader_id_of_class(lookup) == ctx.loader_id_of_class(declaring)
    {
        return None;
    }
    // `restrictReceiver` throws "caller class must be a subclass below the
    // method" when the lookup class is not a subclass of the declaring class;
    // `checkAccess` has already refused every such lookup this model knows.
    // Unmodelled: keep the unrestricted type.
    if !ctx.is_subclass(lookup, declaring) {
        return None;
    }
    Some((lookup_name, lookup))
}

/// Stamp `type_desc` as `mh`'s `type` (the JDK's `restrictReceiver` view);
/// `MH_DESC` is left as the member's own descriptor.
pub(crate) fn lk_restrict_receiver_type(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    type_desc: &str,
) -> MethodCallResult {
    let pin = ctx.pin_native_root(mh);
    let mt = build_method_type_from_descriptor(ctx, type_desc);
    let mh = ctx.read_native_pin(pin, mh);
    ctx.unpin_native_roots(pin);
    if let Some(mt) = mt? {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    Ok(Some(Value::Object(Some(mh))))
}

pub(crate) fn lookup_find_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(ctx, args, 2, None, true, "putField").map_err(|r| r.into_error(ctx))?;
    lk_enforce_lookup_class_field_access(ctx, args, "putField")?;
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let type_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    if let Some(proxy) = lk_lambda_proxy_of_mirror(&*ctx, class_obj) {
        return lk_lambda_proxy_field_find(ctx, args, proxy, name_obj, type_obj, true);
    }
    // Stage 2's owner (wave 63, `CRATONVM_MH_OWNER_ID_AUTHORITY`): `refc`'s id,
    // read while `class_obj` is current.
    let field_owner = mh_field_owner_of_mirror(&*ctx, Some(class_obj));
    // `refc` itself (i63-L4), read while `class_obj` is current.
    let mirror_cid = ctx.class_id_from_mirror(class_obj);
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    // Read before the load below, a GC point for every reference in `args`.
    let strict = mhn_strict_type_descriptor(&*ctx, type_obj);
    let from = lk_from_of(&*ctx, args);
    // Instance field: LOAD only, a receiver initializes the class.
    let target_cid = lk_field_finder_refc(ctx, mirror_cid, &class);
    let finder = LkFieldFinder::Setter;
    if !lk_field_resolution(
        ctx,
        finder,
        target_cid,
        &class,
        &name,
        &field_desc,
        strict.as_deref(),
        from,
    )? {
        // Same evidence rule as `lookup_find_getter` above.
        lookup_require_field(
            ctx,
            target_cid,
            &class,
            &name,
            &field_desc,
            finder.requested_ref_kind(),
        )?;
        if let Some(e) =
            lk_field_mismatch(ctx, finder, target_cid, &class, &name, &field_desc, from)
        {
            return Err(e);
        }
    }
    let desc = format!("(L{class};{field_desc})V");
    // `getDirectFieldCommon` restricts a setter's receiver as it does a
    // getter's (wave 44, `L4W44ProtectedReceiver` row `setter`).
    let restricted = lk_protected_receiver(&*ctx, from, target_cid, LkMember::Field(&name));
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_SETTER)?;
    mh_record_instance_owner(&*ctx, mh, field_owner);
    mh_record_minted_field_slot(&*ctx, mh, field_owner, &name);
    if let Some(receiver) = restricted {
        return lk_restrict_receiver_type(ctx, mh, &format!("(L{receiver};{field_desc})V"));
    }
    Ok(Some(Value::Object(Some(mh))))
}

/// The empty handle `findStaticGetter` / `findStaticSetter` answer for a null
/// argument (their long-standing permissive shape, kept as is; the other
/// finders raise [`lookup_null_arg`]). One allocation, and the caller returns
/// it at once.
pub(crate) fn lookup_placeholder_handle(ctx: &mut dyn NativeContext, kind: i32) -> MethodCallResult {
    let mh = alloc_method_handle(ctx, "", "", "", kind)?;
    Ok(Some(Value::Object(Some(mh))))
}

pub(crate) fn lookup_find_static_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(ctx, args, 2, None, true, "getStatic").map_err(|r| r.into_error(ctx))?;
    lk_enforce_lookup_class_field_access(ctx, args, "getStatic")?;
    // A null argument is a caller bug: `NullPointerException`, as HotSpot's
    // `resolveOrFail` raises it (see `lookup_null_arg`). This finder handed
    // back an inert handle (a null VarHandle) until wave 17.
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let type_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    // Every read of `args` before `lk_field_resolution`, which can run Java
    // (the refusal's `Module.toString()`).
    let strict = mhn_strict_type_descriptor(&*ctx, type_obj);
    let from = lk_from_of(&*ctx, args);
    let class_id = ctx.class_id_from_mirror(class_obj);
    let owner = mh_static_owner_of_mirror(&*ctx, Some(class_obj));
    // `findStaticGetter` on an INSTANCE field is `IllegalAccessException`, and a
    // descriptor mismatch is `NoSuchFieldException` -- see `lk_field_mismatch`.
    // An absent field is `NoSuchFieldException` when resolution proves it
    // (wave 24; a handle came back).
    let finder = LkFieldFinder::StaticGetter;
    if !lk_field_resolution(
        ctx,
        finder,
        class_id,
        &class,
        &name,
        &field_desc,
        strict.as_deref(),
        from,
    )? {
        if let Some(e) = lk_field_mismatch(ctx, finder, class_id, &class, &name, &field_desc, from)
        {
            return Err(e);
        }
    }
    let desc = format!("(){field_desc}");
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_GETTER)?;
    mh_record_static_owner(ctx, mh, owner);
    Ok(Some(Value::Object(Some(mh))))
}

pub(crate) fn lookup_find_static_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(ctx, args, 2, None, true, "putStatic").map_err(|r| r.into_error(ctx))?;
    lk_enforce_lookup_class_field_access(ctx, args, "putStatic")?;
    // A null argument is a caller bug: `NullPointerException`, as HotSpot's
    // `resolveOrFail` raises it (see `lookup_null_arg`). This finder handed
    // back an inert handle (a null VarHandle) until wave 17.
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let type_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    // Every read of `args` before `lk_field_resolution` (see the getter).
    let strict = mhn_strict_type_descriptor(&*ctx, type_obj);
    let from = lk_from_of(&*ctx, args);
    let class_id = ctx.class_id_from_mirror(class_obj);
    let owner = mh_static_owner_of_mirror(&*ctx, Some(class_obj));
    // This finder checked nothing before wave 24: a missing field, a field of
    // another type, an instance field and a `static final` field all came
    // back as a handle (HotSpot: `NoSuchFieldException` /
    // `IllegalAccessException`, `L4W24LookupFinderMessages`).
    let finder = LkFieldFinder::StaticSetter;
    if !lk_field_resolution(
        ctx,
        finder,
        class_id,
        &class,
        &name,
        &field_desc,
        strict.as_deref(),
        from,
    )? {
        if let Some(e) = lk_field_mismatch(ctx, finder, class_id, &class, &name, &field_desc, from)
        {
            return Err(e);
        }
    }
    let desc = format!("({field_desc})V");
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_SETTER)?;
    mh_record_static_owner(ctx, mh, owner);
    Ok(Some(Value::Object(Some(mh))))
}

pub(crate) fn lookup_find_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(ctx, args, 2, None, true, "getField").map_err(|r| r.into_error(ctx))?;
    lk_enforce_lookup_class_field_access(ctx, args, "getField")?;
    // A null argument is a caller bug: `NullPointerException`, as HotSpot's
    // `resolveOrFail` raises it (see `lookup_null_arg`). This finder handed
    // back an inert handle (a null VarHandle) until wave 17.
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let type_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    // A lambda proxy's captured field (interpreter round i3 wave 79, lane L4):
    // the proxy's mirror names no class, so the path below minted over the
    // class `""` with no coordinate (`coordinateTypes()` refused, `kind 0`).
    if let Some(proxy) = lk_lambda_proxy_of_mirror(&*ctx, class_obj) {
        let field_name = ctx.read_string(name_obj).unwrap_or_default();
        let field_desc = field_descriptor_from_mirror(&*ctx, type_obj).into_owned();
        return lk_lambda_proxy_var_handle(ctx, proxy, &field_name, &field_desc);
    }
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let field_name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    // Read before the load below, a GC point for every reference in `args`.
    let strict = mhn_strict_type_descriptor(&*ctx, type_obj);
    let from = lk_from_of(&*ctx, args);

    let class_id = match mirror_class_id(ctx, class_obj) {
        Some(id) => id,
        None => match ctx.class_id_by_name(&class) {
            Some(id) => id,
            None => {
                // Try to load the class (LOAD only: an instance field's
                // receiver initializes it).
                match lookup_load_class(ctx, &class) {
                    Ok(Some(id)) => id,
                    _ => return Ok(Some(Value::Object(None))),
                }
            }
        },
    };
    // HotSpot raises `NoSuchFieldException` from `findVarHandle` for a field
    // that is not there. `findGetter`/`findSetter` two functions up already run
    // this exact gate; `findVarHandle` did not, and the `.unwrap_or(0)` below
    // then turned the miss into a handle onto SLOT 0 — so
    // `findVarHandle(Point.class, "z", int.class).set(p, 42)` silently
    // overwrote `Point.x`. `lookup_require_field` only refuses when it could
    // actually enumerate the declared fields, which keeps the stub-class escape
    // hatch its two existing callers depend on. Since wave 24 a proven
    // resolution answers first, so a field of another type or of the other
    // static-ness is refused too (`L4W24LookupFinderMessages`).
    let finder = LkFieldFinder::VarHandle;
    if !lk_field_resolution(
        ctx,
        finder,
        Some(class_id),
        &class,
        &field_name,
        &field_desc,
        strict.as_deref(),
        from,
    )? {
        lookup_require_field(
            ctx,
            Some(class_id),
            &class,
            &field_name,
            &field_desc,
            finder.requested_ref_kind(),
        )?;
    }
    // `-1`, not `0`, is "unresolved" — see `alloc_instance_var_handle`. A `0`
    // here is indistinguishable from a real first field, so the by-name
    // re-resolution in `varhandle_get`/`varhandle_set` could never fire.
    let field_index = match ctx.resolve_field_index(&class, &field_name) {
        Some(idx) => idx as i32,
        None => -1,
    };
    // A protected field of another package found through a subclass's lookup:
    // the handle's coordinate is the lookup class (wave 45).
    let (coord_class, coord_id) = vh_protected_coordinate(&*ctx, from, class_id, &field_name)
        .unwrap_or_else(|| (class.clone(), class_id));

    let vh = alloc_instance_var_handle(
        ctx,
        &coord_class,
        &field_name,
        &field_desc,
        field_index,
        coord_id,
        Some(class_id),
    )?;
    // A `final` field yields a READ-ONLY handle. Decided here, once, because
    // this is where the field is already resolved.
    let vh = if vh_field_is_final(ctx, class_id, &field_name) {
        vh_mark_read_only(ctx, vh)
    } else {
        vh
    };
    Ok(Some(Value::Object(Some(vh))))
}

pub(crate) fn lookup_find_static_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_enforce_find_access(ctx, args, 2, None, true, "getStatic").map_err(|r| r.into_error(ctx))?;
    lk_enforce_lookup_class_field_access(ctx, args, "getStatic")?;
    // A null argument is a caller bug: `NullPointerException`, as HotSpot's
    // `resolveOrFail` raises it (see `lookup_null_arg`). This finder handed
    // back an inert handle (a null VarHandle) until wave 17.
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("refc")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("name")),
    };
    let type_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(lookup_null_arg("type")),
    };
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let field_name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    // Every read of `args` before `lk_field_resolution` (see the getter).
    let strict = mhn_strict_type_descriptor(&*ctx, type_obj);
    let from = lk_from_of(&*ctx, args);
    let class_id = ctx.class_id_from_mirror(class_obj);

    let owner = mh_static_owner_of_mirror(&*ctx, Some(class_obj));
    // Wave 24: a PROVEN absent field, one of another type, or an instance
    // field is refused as HotSpot refuses it; this finder checked nothing
    // before. Unprovable stays permissive (no legacy check existed here).
    lk_field_resolution(
        ctx,
        LkFieldFinder::StaticVarHandle,
        class_id,
        &class,
        &field_name,
        &field_desc,
        strict.as_deref(),
        from,
    )?;
    let mut vh = alloc_static_var_handle(ctx, &class, &field_name, &field_desc, owner)?;
    // Same rule for a `static final`. Interpreter round i4 wave 84 (lane L4):
    // asked of the class the MIRROR names (a by-name lookup misses a class
    // only a child loader defines, and could find another loader's class),
    // and of the field's DECLARING class as JVMS 5.4.3.2 finds it -- the
    // named class, its superinterfaces, then its superclasses. The walk
    // `vh_field_is_final` makes is superclasses only, so an interface
    // constant reached through an implementing class
    // (`findStaticVarHandle(Impl.class, "K", int.class)`) got a WRITABLE
    // handle whose `set` overwrote the constant; HotSpot answers a read-only
    // handle (`set` -> `UnsupportedOperationException`).
    // `L4W84VarHandleModeEdges` row `iface-const`.
    if let Some(cid) = class_id.or_else(|| ctx.class_id_by_name(&class)) {
        let holder = static_field_holder(&*ctx, cid, &field_name).unwrap_or(cid);
        if vh_field_is_final(ctx, holder, &field_name) {
            vh = vh_mark_read_only(ctx, vh);
        }
    }
    Ok(Some(Value::Object(Some(vh))))
}

/// Is `mh` (a static, virtual or constructor shim with this model's slots) an
/// `asType` copy whose `type` is not its member's own? HotSpot's `asType`
/// answers a `BoundMethodHandle` / LambdaForm handle with no member, which
/// `revealDirect` refuses as `not a direct method handle` (measured, HotSpot
/// 25, `L4W25RevealDirect` row `adapted`); this model's `asType` stamps a
/// copy carrying the direct handle's slots, so the copy is told apart by its
/// `type` only. Two copies keep the direct answer: a variable-arity handle
/// (whose adaptations this does not model), and a virtual handle whose
/// receiver was narrowed to a SUBCLASS of `refc` with the rest of the type
/// unchanged, which is how `findVirtual` restricts a protected member's
/// receiver to the lookup class (HotSpot's `viewAsType` keeps that handle
/// direct). Interpreter round i1 wave 48, lane L4.
pub(crate) fn reveal_sees_an_adapted_copy(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    class: &str,
    desc: &str,
) -> bool {
    if !matches!(kind, MH_KIND_STATIC | MH_KIND_VIRTUAL | MH_KIND_CONSTRUCTOR)
        || mh_is_varargs_collector(&*ctx, mh)
    {
        return false;
    }
    let Some(own_type) = direct_handle_own_type(kind, class, desc) else {
        return false;
    };
    let Some(declared) = mh_declared_descriptor(ctx, mh) else {
        return false;
    };
    if declared == own_type {
        return false;
    }
    if kind == MH_KIND_VIRTUAL {
        // `(L<recv>;<rest>` with `<rest>` the member's own: a restricted
        // receiver when `<recv>` is a proper subclass of `refc`.
        let rest = desc.strip_prefix('(').unwrap_or(desc);
        let narrowed = declared
            .strip_prefix("(L")
            .and_then(|d| d.strip_suffix(rest))
            .and_then(|d| d.strip_suffix(';'))
            .and_then(|recv| Some((ctx.class_id_by_name(recv)?, ctx.class_id_by_name(class)?)))
            .is_some_and(|(recv, refc)| recv != refc && ctx.is_subclass(recv, refc));
        if narrowed {
            return false;
        }
    }
    true
}

/// The class that declares the member a direct handle over `refc` names, or
/// `None` to keep `refc`: a method (`desc` without the receiver) found on
/// `refc`'s superclass chain, else, for an interface `refc`, a public
/// `java/lang/Object` method; a field found on the superclass chain or, for a
/// static constant, a superinterface. Bounded walks. Interpreter round i1
/// wave 48, lane L4 (`revealDirect` reported `refc`).
pub(crate) fn reveal_declaring_class(
    ctx: &dyn NativeContext,
    refc: &str,
    name: &str,
    desc: &str,
    is_field: bool,
) -> Option<String> {
    let start = ctx.class_id_by_name(refc)?;
    declaring_class_id(ctx, start, name, desc, is_field).and_then(|cid| ctx.class_name_of_id(cid))
}

/// [`reveal_declaring_class`] by class id: the class a member named through
/// `start` resolves to when that is not `start` itself, else `None`. A method
/// is the holder [`mhn_method_resolution`] finds (JVMS 5.4.3.3 / 5.4.3.4
/// order), as [`resolved_method_holder`] keeps it (interpreter round i3 wave
/// 83). A field: superclasses, then superinterfaces. Reads only; bounded.
pub(crate) fn declaring_class_id(
    ctx: &dyn NativeContext,
    start: ClassId,
    name: &str,
    desc: &str,
    is_field: bool,
) -> Option<ClassId> {
    if !is_field {
        return match mhn_method_resolution(ctx, start, name, desc) {
            (MhnVerdict::Found(_), Some(holder)) => resolved_method_holder(ctx, start, holder),
            _ => None,
        };
    }
    let declares = |cid: ClassId| {
        mhn_declared_fields_named(&*ctx, cid, name)
            .0
            .iter()
            .any(|f| f.name == name)
    };
    let mut cur = Some(start);
    for _ in 0..64 {
        let Some(cid) = cur else { break };
        if declares(cid) {
            return if cid == start { None } else { Some(cid) };
        }
        cur = ctx.superclass_of(cid).filter(|&sup| sup != cid);
    }
    // A constant of a superinterface (breadth-first, bounded).
    let mut pending: Vec<ClassId> = ctx.class_interfaces(start);
    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(iface) = pending.pop() {
        if seen.contains(&iface) || seen.len() > 256 {
            continue;
        }
        seen.push(iface);
        if declares(iface) {
            return Some(iface);
        }
        pending.extend(ctx.class_interfaces(iface));
    }
    None
}

/// The class a method member named through `named` and declared by `holder`
/// resolves to, as HotSpot's `init_method_MemberName` writes it, or `None`
/// when that is `named` itself: the holder, except that a default method a
/// CLASS inherits from an interface keeps the class (HotSpot's vtable call
/// answers the resolved class). Through an interface the holder is kept, a
/// superinterface or `Object` (interpreter round i3 wave 83).
pub(crate) fn resolved_method_holder(
    ctx: &dyn NativeContext,
    named: ClassId,
    holder: ClassId,
) -> Option<ClassId> {
    let keeps_named =
        holder == named || (!ctx.is_interface_class(named) && ctx.is_interface_class(holder));
    (!keeps_named).then_some(holder)
}

/// What a direct handle of this model cracks as: the member
/// `revealDirect` reports and `internalMemberName()` answers (interpreter
/// round i1 wave 49, lane L4, factored out of [`lookup_reveal_direct`]).
pub(crate) struct DirectHandleMember {
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    pub(crate) ref_kind: i32,
    pub(crate) kind_flag: i32,
    pub(crate) acc_static: i32,
    /// A lambda proxy's member (`class` is `""`): the proxy, whose mirror is
    /// the `MemberName`'s `clazz` (interpreter round i2 wave 62, lane L4;
    /// `CRATONVM_MH_INSTANCE_OWNER_ID`).
    pub(crate) proxy: Option<ClassId>,
}

/// The lambda proxy that unbound instance handle `mh` (minted over the class
/// `""`) was recorded against, or `None`. Under `CRATONVM_MH_INSTANCE_OWNER_ID`.
pub(crate) fn mh_lambda_proxy_owner(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<ClassId> {
    let owner = mh_instance_owner(ctx, mh)?;
    crate::lang_class::lambda_proxy_class_name(ctx, owner).map(|_| owner)
}

/// The member the handle `mh` of this model cracks as, or HotSpot's
/// `IllegalArgumentException: not a direct method handle` for a bound,
/// adapted or combinator handle. Allocates nothing.
pub(crate) fn direct_handle_member(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Result<DirectHandleMember, MethodCallFailed> {
    // MemberName flag bits (mirroring native_mhn_init).
    const IS_METHOD: i32 = 0x1_0000;
    const IS_CONSTRUCTOR: i32 = 0x2_0000;
    const IS_FIELD: i32 = 0x4_0000;
    // JVM spec table 5.4.3.5 reference kinds.
    const REF_GET_FIELD: i32 = 1;
    const REF_GET_STATIC: i32 = 2;
    const REF_PUT_FIELD: i32 = 3;
    const REF_PUT_STATIC: i32 = 4;
    const REF_INVOKE_VIRTUAL: i32 = 5;
    const REF_INVOKE_STATIC: i32 = 6;
    const REF_INVOKE_SPECIAL: i32 = 7;
    const REF_NEW_INVOKE_SPECIAL: i32 = 8;
    const REF_INVOKE_INTERFACE: i32 = 9;
    const ACC_STATIC: i32 = 0x0008;

    // Read our private slots. If the MH was not produced by our find* path
    // (foreign MH — bound, adapted, asType-converted) these slots will be
    // empty, in which case we fall back to a default `()V`/static descriptor
    // so revealDirect at least returns a non-throwing InfoFromMemberName.
    let class = mh_read_class(ctx, mh).unwrap_or_default();
    let name = mh_read_name(ctx, mh).unwrap_or_default();
    let desc = mh_read_desc(ctx, mh).unwrap_or_else(|| DESC_DEFAULT_METHOD.to_string());
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_STATIC,
    };
    // A lambda proxy's member handle (wave 62, lane L4): minted over the
    // class `""`, it cracks as the proxy's own member, as HotSpot's
    // `revealDirect(findVirtual(P, "run", ..))` answers `invokeVirtual P.run`.
    let proxy = if class.is_empty() {
        mh_lambda_proxy_owner(&*ctx, mh)
    } else {
        None
    };
    if proxy.is_some() {
        let bound = matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)));
        let instance_kind = matches!(kind, MH_KIND_VIRTUAL | MH_KIND_GETTER | MH_KIND_SETTER);
        if bound || !instance_kind {
            return Err(RuntimeError::IllegalArgumentException {
                message: "not a direct method handle".to_string(),
            }
            .into());
        }
    }
    // HotSpot cracks a DIRECT handle only (wave 25, probe
    // `tools/probes/interp/L4/L4W25RevealDirect.java`): a bound handle
    // (`bindTo`, `Lookup.bind`) or a combinator is `IllegalArgumentException:
    // not a direct method handle`. This native answered both with the
    // member underneath (a bound one) or with a `()V` static member invented
    // from the combinator's slots. Decided only for a handle minted with our
    // slots (`MH_CLASS` present): a handle without them keeps the permissive
    // answer below, since nothing here can tell what it is.
    if !class.is_empty() {
        let direct_kind = matches!(
            kind,
            MH_KIND_STATIC
                | MH_KIND_VIRTUAL
                | MH_KIND_SPECIAL
                | MH_KIND_CONSTRUCTOR
                | MH_KIND_GETTER
                | MH_KIND_SETTER
        );
        // A constructor handle keeps its class id in `MH_BOUND` as an `Int`;
        // only a bound RECEIVER / argument is an object there.
        let bound = matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)));
        if !direct_kind || bound || reveal_sees_an_adapted_copy(ctx, mh, kind, &class, &desc) {
            return Err(RuntimeError::IllegalArgumentException {
                message: "not a direct method handle".to_string(),
            }
            .into());
        }
    } else if proxy.is_none() && method_handle_value_not_direct(ctx, mh).is_some() {
        // A combinator minted without a class (`constant`, `identity`,
        // `throwException`, the adapters): HotSpot has no member to crack
        // (wave 48, `L4W48ThrowException` row `reveal`); this answered a
        // `()V` static member invented from the empty slots.
        return Err(RuntimeError::IllegalArgumentException {
            message: "not a direct method handle".to_string(),
        }
        .into());
    }

    // Map our MH_KIND_* to (refKind, kindFlag, accStaticBits).
    let (ref_kind, kind_flag, acc_static) = match kind {
        MH_KIND_STATIC => (REF_INVOKE_STATIC, IS_METHOD, ACC_STATIC),
        // `findVirtual` on an interface is `REF_invokeInterface`, which is
        // what HotSpot's info reports (wave 25); on a class (even for a
        // default method it inherits) `REF_invokeVirtual`.
        MH_KIND_VIRTUAL
            if ctx
                .class_id_by_name(&class)
                .is_some_and(|cid| ctx.is_interface_class(cid)) =>
        {
            (REF_INVOKE_INTERFACE, IS_METHOD, 0)
        }
        MH_KIND_VIRTUAL => (REF_INVOKE_VIRTUAL, IS_METHOD, 0),
        MH_KIND_SPECIAL => (REF_INVOKE_SPECIAL, IS_METHOD, 0),
        MH_KIND_CONSTRUCTOR => (REF_NEW_INVOKE_SPECIAL, IS_CONSTRUCTOR, 0),
        MH_KIND_GETTER => {
            // Distinguish static vs instance by descriptor arity.
            let is_static = desc.starts_with("()");
            let rk = if is_static {
                REF_GET_STATIC
            } else {
                REF_GET_FIELD
            };
            (rk, IS_FIELD, if is_static { ACC_STATIC } else { 0 })
        }
        MH_KIND_SETTER => {
            let is_static = !desc_has_two_params(&desc);
            let rk = if is_static {
                REF_PUT_STATIC
            } else {
                REF_PUT_FIELD
            };
            (rk, IS_FIELD, if is_static { ACC_STATIC } else { 0 })
        }
        _ => (REF_INVOKE_STATIC, IS_METHOD, ACC_STATIC),
    };
    // The member's DECLARING class, as HotSpot's resolved `MemberName` has
    // it (wave 48): `findVirtual(Sub, m)` of `Base.m` reveals
    // `invokeVirtual Base.m`, a field or static likewise, and an interface's
    // `findVirtual` of an `Object` method `invokeVirtual Object.hashCode`.
    // `MH_CLASS` is the finder's `refc`. A default method a class inherits
    // stays the class's (HotSpot's too).
    let class = if kind_flag == IS_CONSTRUCTOR || proxy.is_some() {
        class
    } else {
        reveal_declaring_class(&*ctx, &class, &name, &desc, kind_flag == IS_FIELD).unwrap_or(class)
    };
    let ref_kind = if ref_kind == REF_INVOKE_INTERFACE && class == "java/lang/Object" {
        REF_INVOKE_VIRTUAL
    } else {
        ref_kind
    };

    Ok(DirectHandleMember {
        class,
        name,
        desc,
        ref_kind,
        kind_flag,
        acc_static,
        proxy,
    })
}

/// A resolved `MemberName` for `member` (clazz, name, type, flags with the
/// member's own modifiers and reference kind), as HotSpot's direct handle
/// holds it. Factored out of [`lookup_reveal_direct`] (wave 49, lane L4).
/// The answer is unpinned: the caller pins it before its next GC point.
pub(crate) fn build_direct_member_name(
    ctx: &mut dyn NativeContext,
    member: &DirectHandleMember,
) -> Result<ObjectRef, MethodCallFailed> {
    const IS_FIELD: i32 = 0x4_0000;
    const ACC_PUBLIC: i32 = 0x0001;
    let DirectHandleMember {
        class,
        name,
        desc,
        ref_kind,
        kind_flag,
        acc_static,
        proxy,
    } = member;
    let (class, name, desc) = (class.as_str(), name.as_str(), desc.as_str());
    let (ref_kind, kind_flag, acc_static, proxy) = (*ref_kind, *kind_flag, *acc_static, *proxy);
    // Build the MemberName. Its declared layout is documented on the `MN_*`
    // constants next to `native_mhn_resolve`.
    let mn = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MemberName", MN_FIELD_COUNT)?;
    let mn_pin = ctx.pin_native_root(mn);

    // clazz: Class mirror of the declaring class. Fall back to a synthetic
    // mirror only if the class is genuinely unloadable.
    let clazz_mirror = match proxy.or_else(|| ctx.class_id_by_name(&class)) {
        Some(cid) => ctx.get_class_mirror(cid),
        None => {
            // `revealDirect` initializes nothing on HotSpot: load only.
            let _ = lookup_load_class(ctx, &class);
            match ctx.class_id_by_name(&class) {
                Some(cid) => ctx.get_class_mirror(cid),
                None => try_alloc_concurrent_synthetic(ctx, "java/lang/Class", 1)?,
            }
        }
    };
    let mn = ctx.read_native_pin(mn_pin, mn);
    mn_set(
        ctx,
        mn,
        "clazz",
        MN_CLAZZ,
        Value::Object(Some(clazz_mirror)),
    );

    // name (re-read: the string allocation can collect, as in
    // `alloc_resolved_member_name`)
    let name_str = ctx.create_string(&name);
    let mn = ctx.read_native_pin(mn_pin, mn);
    mn_set(ctx, mn, "name", MN_NAME, Value::Object(Some(name_str)));

    // type: MethodType for methods/constructors; Class for fields. This used to
    // write the named slot AND raw slot 2, "so JDK code that accesses `type`
    // via either route sees the populated value". They are the same slot on
    // both layouts, and `mn_set` picks whichever route the receiver actually
    // has — the by-name write is a silent no-op on a fabricated layout, and the
    // raw write is what a real one needs resolved on the receiver.
    let type_value = if kind_flag == IS_FIELD {
        let field_type_slice = field_type_from_desc(&desc, ref_kind);
        Value::Object(Some(field_type_mirror(ctx, &field_type_slice)?))
    } else {
        let mt = match build_method_type_from_descriptor(ctx, &desc)? {
            Some(mt) => Some(mt),
            None => build_method_type_from_descriptor(ctx, "()V")?,
        };
        Value::Object(mt)
    };
    // gen r4w3/rooting: re-read after the mirror / MethodType build.
    let mn = ctx.read_native_pin(mn_pin, mn);
    mn_set(ctx, mn, "type", MN_TYPE, type_value);

    // flags: kind_flag | (modifiers) | (refKind << 24). The modifiers are the
    // resolved member's own access flags (wave 25): `getModifiers()` /
    // `isVarArgs()` / `toString()` read them, and HotSpot's `MemberName` has
    // them from resolution -- `private`, `protected final synchronized`,
    // `static native`, a varargs method's `ACC_VARARGS`, a package-private
    // constructor or field (`L4W25RevealDirect`). This native answered
    // `public` (plus `static`) for every member. The search is the one
    // `MethodHandleNatives.resolve` uses (`mhn_method_verdict` /
    // `mhn_field_verdict`, which find an inherited default method or an
    // interface constant), then the superclass-only walks; a member none of
    // them finds keeps the old `public` answer.
    let declared_access: Option<u16> = match proxy {
        // A lambda proxy's member (wave 62): its own method or captured field.
        Some(proxy) if kind_flag == IS_FIELD => {
            crate::lang_class::declared_fields_with_aliases(&*ctx, proxy)
                .into_iter()
                .find(|f| !f.is_static && f.name == name)
                .map(|f| f.access_flags)
        }
        Some(proxy) => lk_lambda_proxy_method_access(&*ctx, proxy, name, desc),
        None => match ctx.class_id_by_name(&class) {
            Some(cid) if kind_flag == IS_FIELD => {
                let field_desc = field_type_from_desc(&desc, ref_kind);
                match mhn_field_verdict(&*ctx, cid, &name, &field_desc) {
                    MhnVerdict::Found(access) => Some(access),
                    _ => declared_field_access_flags(ctx, cid, &name),
                }
            }
            Some(cid) => match mhn_method_verdict(&*ctx, cid, &name, &desc) {
                MhnVerdict::Found(access) => Some(access),
                _ => declared_method_access_flags(ctx, cid, &name, &desc),
            },
            None => None,
        },
    };
    let modifiers = declared_access.map_or(ACC_PUBLIC | acc_static, i32::from);
    let flags = kind_flag | modifiers | (ref_kind << 24);
    // `declared_methods` / `declared_fields` allocate: re-read the pinned
    // MemberName before writing through it.
    let mn = ctx.read_native_pin(mn_pin, mn);
    mn_set(ctx, mn, "flags", MN_FLAGS, Value::Int(flags));

    // vmindex sentinel — our layout only; see `mn_set_vmindex`.
    mn_set_vmindex(ctx, mn, 1);
    // `resolution == null` marks the MemberName resolved per the JDK.
    mn_set(ctx, mn, "resolution", MN_RESOLUTION, Value::Object(None));
    let mn = ctx.read_native_pin(mn_pin, mn);
    ctx.unpin_native_roots(mn_pin);
    Ok(mn)
}

/// `MethodHandle.internalMemberName()` (interpreter round i1 wave 49, lane
/// L4; `interpreter-L4-constantbootstraps-getstaticfinal-reads-no-membername-FIXED-20261013`): for a
/// field getter or setter this model minted (class exactly
/// `java/lang/invoke/MethodHandle`, our slots present, direct), the resolved
/// `MemberName` HotSpot's `DirectMethodHandle` holds, as `revealDirect`
/// builds it; for every other receiver the base class's own bytecode
/// (`null`, as the JDK answers for a bound or adapted handle). Method and
/// constructor handles keep `null` for now: a member there changes the JDK
/// paths of `MethodHandleImpl.bindCaller`, `LambdaForm.NamedFunction` and
/// `withInternalMemberName` (census before widening).
pub(crate) fn method_handle_internal_member_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(mh))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let carrier = ctx.class_id_by_name("java/lang/invoke/MethodHandle");
    let is_field_carrier = carrier.is_some_and(|cid| ctx.class_id_of_object(mh) == cid)
        && matches!(
            ctx.get_field(mh, MH_KIND),
            Value::Int(MH_KIND_GETTER | MH_KIND_SETTER)
        )
        && mh_read_class(ctx, mh).is_some_and(|c| !c.is_empty());
    if is_field_carrier {
        if let Ok(member) = direct_handle_member(ctx, mh) {
            let mn = build_direct_member_name(ctx, &member)?;
            return Ok(Some(Value::Object(Some(mn))));
        }
    }
    ctx.invoke_virtual_bytecode_only(
        mh,
        "internalMemberName",
        "()Ljava/lang/invoke/MemberName;",
        &[],
    )
}

/// `MethodHandles.Lookup.revealDirect(MethodHandle)`
///
/// Build a `MemberName` populated from our private MH synthetic slots
/// (MH_CLASS, MH_NAME, MH_DESC, MH_KIND) and wrap it in an
/// `InfoFromMemberName`. Bypasses the default Java path which would call
/// `mh.isCrackable()` / `mh.internalMemberName()` — both of which return
/// `false` / `null` on a base `MethodHandle`, raising
/// `IllegalArgumentException: not a direct method handle`.
pub(crate) fn lookup_reveal_direct(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Lookup), args[1] = MethodHandle target
    let mh = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // Null MH — return null rather than throw; callers (LambdaMetafactory
            // and direct revealDirect tests) always pass a real MH.
            return Ok(Some(Value::Object(None)));
        }
    };
    let member = direct_handle_member(ctx, mh)?;
    let ref_kind = member.ref_kind;

    // gen r4w3/rooting: `args[0]` (the Lookup) is read after several GC
    // points below (the synthetic allocations, `<clinit>`, the MethodType
    // build) and a re-read of the slice is stale too — pin a copy now, before
    // the first of them. `mn` and `info` are pinned as they are produced.
    // `rd_base` (the lowest handle) releases everything at the end.
    let lookup_this = args.first().copied().unwrap_or(Value::Object(None));
    let lookup_pin = pin_mh_value(ctx, lookup_this);

    let mn = build_direct_member_name(ctx, &member)?;
    let mn_pin = ctx.pin_native_root(mn);
    let rd_base = if lookup_pin != usize::MAX {
        lookup_pin
    } else {
        mn_pin
    };

    // Construct InfoFromMemberName(Lookup, MemberName, byte). Field layout
    // (verified via javap): { MemberName member, int referenceKind }. We
    // bypass <init> assertions by direct field writes — they're disabled in
    // production JDKs anyway.
    let info = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/InfoFromMemberName", 2)?;
    // gen r4w3/rooting: `info` survives the `<init>` invoke below; `mn` and
    // the Lookup are re-read after the allocation.
    let info_pin = ctx.pin_native_root(info);
    let mn = ctx.read_native_pin(mn_pin, mn);
    let lookup_this = read_pinned_mh_arg(ctx, lookup_pin, lookup_this);
    // Invoke the real constructor so any future-version field additions are
    // populated correctly. Falls back to direct field writes if invocation
    // fails (e.g. class not yet on the classpath in stripped runtimes).
    let init_args = [
        Value::Object(Some(info)),
        lookup_this,
        Value::Object(Some(mn)),
        Value::Int(ref_kind),
    ];
    let _ = ctx.invoke(
        "java/lang/invoke/InfoFromMemberName",
        "<init>",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/invoke/MemberName;B)V",
        &init_args,
    );
    // Defensive: also write fields directly, so even if <init> was skipped
    // (registered as a no-op native somewhere) `member`/`referenceKind` are
    // populated and downstream `toString`/`getDeclaringClass` work. Write
    // both by-name and by raw declared slot index.
    // gen r4w3/rooting: re-read both after the `<init>` invoke.
    let info = ctx.read_native_pin(info_pin, info);
    let mn = ctx.read_native_pin(mn_pin, mn);
    ctx.unpin_native_roots(rd_base);
    ctx.set_field_by_name(info, "member", Value::Object(Some(mn)));
    ctx.set_field_by_name(info, "referenceKind", Value::Int(ref_kind));
    ctx.set_field(info, 0, Value::Object(Some(mn)));
    ctx.set_field(info, 1, Value::Int(ref_kind));

    Ok(Some(Value::Object(Some(info))))
}

/// Extract the field type descriptor slice from a getter/setter descriptor.
/// Getter: `()T` or `(Lowner;)T`. Setter: `(T)V` or `(Lowner;T)V`.
pub(crate) fn field_type_from_desc(desc: &str, ref_kind: i32) -> String {
    const REF_GET_FIELD: i32 = 1;
    const REF_GET_STATIC: i32 = 2;
    // For getters the field type is the return-type slice.
    if ref_kind == REF_GET_FIELD || ref_kind == REF_GET_STATIC {
        if let Some(idx) = desc_close_paren(desc) {
            return desc[idx + 1..].to_string();
        }
        return DESC_VOID.to_string();
    }
    // For setters the field type is the LAST parameter.
    let end = desc_close_paren(desc).unwrap_or(desc.len());
    let params = desc.get(1..end).unwrap_or("");
    // Walk forward and remember the last full type.
    let bytes = params.as_bytes();
    let mut i = 0;
    let mut last_start = 0;
    while i < bytes.len() {
        last_start = i;
        match bytes[i] as char {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' => {
                i += 1;
            }
            '[' => {
                while i < bytes.len() && bytes[i] as char == '[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] as char == 'L' {
                    while i < bytes.len() && bytes[i] as char != ';' {
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            'L' => {
                while i < bytes.len() && bytes[i] as char != ';' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    params[last_start..].to_string()
}

/// Build a Class mirror for a field-type descriptor slice (one JVM type).
pub(crate) fn field_type_mirror(ctx: &mut dyn NativeContext, ty: &str) -> Result<ObjectRef, MethodCallFailed> {
    // A primitive field's type is the primitive's own mirror (`int.class`),
    // as on HotSpot. This used to be the WRAPPER's mirror as a "proxy", so
    // `revealDirect(findGetter(C, "i", int.class)).getMethodType()` printed
    // `class java.lang.Integer` where HotSpot prints `()int` (wave 25,
    // `tools/probes/interp/L4/L4W25RevealDirect.java`; the accessor-shaped
    // type is built by the `MemberName.getMethodOrFieldType` native).
    let primitive: &str = match ty.chars().next() {
        Some('B') => "byte",
        Some('C') => "char",
        Some('D') => "double",
        Some('F') => "float",
        Some('I') => "int",
        Some('J') => "long",
        Some('S') => "short",
        Some('Z') => "boolean",
        Some('L') => return field_type_mirror_class(ctx, &ty[1..ty.len().saturating_sub(1)]),
        Some('[') => return field_type_mirror_class(ctx, ty),
        _ => return field_type_mirror_class(ctx, "java/lang/Object"),
    };
    Ok(ctx.primitive_class_mirror(primitive))
}

pub(crate) fn field_type_mirror_class(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(cid) = ctx.class_id_by_name(class_name) {
        return Ok(ctx.get_class_mirror(cid));
    }
    // A field type's mirror needs the class loaded, not initialized.
    let _ = lookup_load_class(ctx, class_name);
    if let Some(cid) = ctx.class_id_by_name(class_name) {
        return Ok(ctx.get_class_mirror(cid));
    }
    Ok(try_alloc_concurrent_synthetic(ctx, "java/lang/Class", 1)?)
}

// =============================================================================
// MethodHandles extra factory methods
// =============================================================================

/// Bridge `MethodHandles.arrayElementGetter` / `arrayElementSetter` to a
/// synthetic MethodHandle. Shared by synthetic-mode registration
/// (`register_p65_method_handles_extra`) and the real-JDK essential path
/// (promoted alongside the other real-JDK MethodHandle bridges in `lib.rs`).
///
/// In real-JDK mode the genuine `MethodHandleImpl.makeArrayElementAccessor`
/// bytecode runs but, for primitive arrays, adapts the generic accessor via
/// `MethodHandle.viewAsType` → `MethodHandle.copyWith`, which is abstract (no
/// Code attribute) on CratonVM's synthetic MethodHandles → AbstractMethodError.
/// `findStatic` / `findVirtual` already work via the real DirectMethodHandle
/// path, so bridging just these two factories is enough for
/// `ObjectStreamClass$RecordSupport.<clinit>` — which builds its
/// `PRIM_VALUE_EXTRACTORS` map via `arrayElementGetter(byte[].class)` — to
/// complete. Without it, that clinit aborts and ANY record-class
/// (de)serialization dies with a bogus
/// `no class def found: java/io/ObjectStreamClass$RecordSupport` linkage error.
///
/// NOTE: requires the matching `check_override` allow-list entry in
/// `vm/src/vm/vm_exec.rs` so this native wins over the (broken) JDK bytecode.
///
/// Still needed after `copyWith` (wave 43), read from JDK 25
/// `MethodHandleImpl.makeArrayElementAccessor` (interpreter round i1 wave 44,
/// lane L4, `i43-L4-proposal-retire-the-copywith-workarounds` step 1): after
/// `viewAsType` the accessor is wrapped by `makeIntrinsic` in a
/// `MethodHandleImpl$IntrinsicMethodHandle`, a `DelegatingMethodHandle` whose
/// constructor builds a reinvoker `LambdaForm` and whose invocation goes
/// through it, the real-JDK handle shape the `MH_KIND_*` model cannot invoke
/// (the reason `identity` is pinned too).
// ---------------------------------------------------------------------------
// Argument checks of the forced-native combinators (interpreter round i1
// wave 23, lane L4)
// ---------------------------------------------------------------------------
//
// The combinators below replace JDK bytecode (see each registrar's note), and
// with the bytecode went the JDK's argument checks: `constant(void.class, x)`,
// `identity(void.class)`, `arrayLength(int.class)`, a value of the wrong type
// handed to `constant` / `insertArguments`, and `asCollector` /
// `asSpreader` / `asVarargsCollector` on a non-array type or an impossible
// position were all ACCEPTED, returning a handle HotSpot 25 never builds.
// These helpers restore the refusals with the JDK's own messages
// (`MethodHandles`, `MethodHandle.asCollectorChecks`/`asSpreaderChecks`/
// `spreadArrayChecks`, `MethodHandleImpl.insertArgumentsChecks`,
// `Wrapper.convert`, `Class.cast`), measured by
// `tools/probes/interp/L4/L4W23ForcedCombinators.java`. As with `asType`'s
// refusal, every way of NOT knowing is an accept: a check refuses only on a
// type it could read exactly.

/// `Class.getName()` of the type a field descriptor names: `int`,
/// `java.lang.String`, `[Ljava.lang.String;`.
pub(crate) fn mh_class_get_name(desc: &str) -> String {
    if desc.starts_with('[') {
        return desc.hidden_dotted();
    }
    match desc.strip_prefix('L').and_then(|d| d.strip_suffix(';')) {
        Some(internal) => internal.hidden_dotted(),
        None => descriptor_simple_name(desc).unwrap_or_else(|| desc.to_string()),
    }
}

/// `Class.toString()` of the type a field descriptor names: `int`,
/// `class java.lang.String`, `interface java.lang.Runnable`, `class [I`.
pub(crate) fn mh_class_to_string(ctx: &dyn NativeContext, desc: &str) -> String {
    if is_primitive_descriptor(desc) {
        return mh_class_get_name(desc);
    }
    let is_interface = desc
        .strip_prefix('L')
        .and_then(|d| d.strip_suffix(';'))
        .and_then(|internal| ctx.class_id_by_name(internal))
        .is_some_and(|cid| ctx.is_interface_class(cid));
    format!(
        "{} {}",
        if is_interface { "interface" } else { "class" },
        mh_class_get_name(desc)
    )
}

/// The exact descriptor of a `Class` mirror argument, or `None` when it cannot
/// be named (no refusal may rest on a guess).
///
/// Wave 118, lane L4: a lambda proxy's mirror answers its own descriptor
/// ([`unnameable_mirror_descriptor`]; see [`strict_mirror_descriptor`]): a
/// proxy's name is not a guess.
pub(crate) fn mh_mirror_descriptor_strict(ctx: &dyn NativeContext, arg: Option<&Value>) -> Option<String> {
    match arg {
        Some(Value::Object(Some(mirror))) => match resolve_class_name_robust(ctx, *mirror) {
            Some(n) if !n.is_empty() => Some(class_name_to_descriptor(&n).into_owned()),
            Some(_) => None,
            None => unnameable_mirror_descriptor(ctx, *mirror),
        },
        _ => None,
    }
}

/// The helpful NPE text HotSpot's `Wrapper.convert` raises for a `null` bound
/// to a primitive (`x.getClass()` on the null).
pub(crate) const MH_NULL_PRIMITIVE_NPE: &str = "Cannot invoke \"Object.getClass()\" because \"x\" is null";

pub(crate) fn mh_illegal_argument(message: String) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException { message }.into()
}

/// What binding `value` to a parameter (or constant return) of type `ty`
/// raises on HotSpot, or `None` when it is accepted or cannot be told:
/// a primitive `ty` takes `Wrapper.convert` (a `null` is an NPE; a wrapper
/// that does not widen to `ty`, or a non-wrapper, is a `ClassCastException`),
/// a reference `ty` takes `Class.cast` — decided exactly for a wrapper or a
/// `String` value, whose supertypes are a closed set, and otherwise by
/// `reference_arg_admitted`, the predicate `bindTo` and `invoke` already
/// refuse on (it declines every proxy, stand-in and unresolvable target).
pub(crate) fn mh_bind_value_refusal(
    ctx: &mut dyn NativeContext,
    ty: &str,
    value: Value,
) -> Option<MethodCallFailed> {
    let primitive = matches!(ty, "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D");
    let obj = match value {
        Value::Object(Some(obj)) => obj,
        Value::Object(None) if primitive => {
            return Some(
                RuntimeError::NullPointerException {
                    message: Some(MH_NULL_PRIMITIVE_NPE.to_string()),
                }
                .into(),
            );
        }
        // A null reference, or a raw primitive: nothing to refuse.
        _ => return None,
    };
    if ctx.heap_kind_of(obj) != ObjectKind::Object {
        // An array bound to an ARRAY parameter takes `Class.cast`'s array
        // rule (wave 60, lane L4: `insertArguments(takesStrings, 0, (Object)
        // new Integer[1])` and `bindTo` were accepted). Every other array is
        // left as before.
        if ty.starts_with('[') && ctx.object_is_array(obj) {
            return (reference_array_target_admitted(&*ctx, obj, ty) == Some(false))
                .then(|| reference_cast_failure(ctx, value, ty));
        }
        return None;
    }
    let value_name = ctx.class_name_of_id(ctx.class_id_of_object(obj))?;
    let value_desc = format!("L{value_name};");
    if primitive {
        let wrapper = wrapper_desc_for_primitive(ty)?;
        if primitive_desc_for_wrapper(&value_desc).is_some_and(|src| primitive_widens_to(src, ty)) {
            return None;
        }
        return Some(
            RuntimeError::ClassCastException {
                message: format!(
                    "{} is not compatible with {}",
                    mh_class_to_string(ctx, wrapper),
                    mh_class_to_string(ctx, &value_desc)
                ),
            }
            .into(),
        );
    }
    let accepted = if primitive_desc_for_wrapper(&value_desc).is_some() {
        reference_accepts_wrapper(ty, &value_desc)
    } else if value_name == "java/lang/String" {
        matches!(
            ty,
            "Ljava/lang/String;"
                | "Ljava/lang/Object;"
                | "Ljava/io/Serializable;"
                | "Ljava/lang/Comparable;"
                | "Ljava/lang/CharSequence;"
                | "Ljava/lang/constant/Constable;"
                | "Ljava/lang/constant/ConstantDesc;"
        )
    } else {
        return (reference_arg_admitted(ctx, value, ty) == Some(false))
            .then(|| reference_cast_failure(ctx, value, ty));
    };
    (!accepted).then(|| {
        RuntimeError::ClassCastException {
            message: format!(
                "Cannot cast {} to {}",
                mh_class_get_name(&value_desc),
                mh_class_get_name(ty)
            ),
        }
        .into()
    })
}

/// `asType`'s reference -> primitive step for ONE argument that arrived as a
/// reference where the handle takes the primitive `prim`: the unboxed value,
/// widened to `prim`, or HotSpot's refusal (wave 25,
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// `site_param` is the call site's static type for this argument when the
/// door knows it (`invoke`'s published descriptor; `invokeWithArguments`
/// adapts to `genericMethodType`, all `Object`). It only decides the wording
/// of a `null`'s NPE: a site typed as `prim`'s own wrapper unboxes through
/// `ValueConversions.unbox<W>(W x)` (`x.intValue()` on a null `x`), every
/// other site through `unbox<W>(Object, false)` -> `primitiveConversion`,
/// which returns the null that the `Number` getter is then called on.
///
/// Measured on HotSpot 25.0.3 (`tools/probes/interp/L4/L4W25PrimitiveArraySetter.java`
/// and the rows quoted there): a wrapper whose primitive widens to `prim` is
/// converted; any other wrapper is `Cannot cast <W> to <prim's wrapper>`
/// (`Class.cast`); a non-wrapper `Number` (`BigInteger`, `AtomicInteger`)
/// takes the same `Class.cast`; anything else fails `primitiveConversion`'s
/// `(Number) x` cast, `<X> cannot be cast to java.lang.Number`, whose
/// module/loader clause the VM's cast-message funnel
/// (`runtime::exceptions::hotspot_vm_type_error_message`) adds. So a
/// non-wrapper is refused whatever the answer to "is it a `Number`" -- that
/// question only picks the message, and a context that cannot answer it gets
/// the `(Number)` wording.
///
/// A raw primitive (the site passed it unboxed) is only widened. A value
/// whose class cannot be named keeps the pre-wave-25 behaviour (unbox a
/// wrapper, pass anything else through) rather than refuse on a guess.
/// The frame HotSpot lists on top of [`mh_unbox_for_primitive`]'s `null`
/// refusal through `primitiveConversion`: `ValueConversions.unbox<W>(Object,
/// boolean)` at its `primitiveConversion(..).<x>Value()` call, above the
/// hidden `LambdaForm` frames of the handle the door served (`L3W60NullUnboxFrames`;
/// `DirectMethodHandleAccessor.invoke:104` below it when `Method.invoke` runs
/// the JDK accessor, `CRATONVM_JDK_REFLECTIVE_INVOKE`). Noted for the
/// funnel's mark (`NativeExceptionAccess::note_served_raise_chain`), which the
/// capture decodes at the call of the handle's invoker. Interpreter round i2
/// wave 60, lane L3 (the i45-L3 proposal's frame row). Out of line, on the
/// refusal path.
#[cold]
#[inline(never)]
pub(crate) fn note_null_unbox_chain(ctx: &mut dyn NativeContext, prim: &str) {
    note_unbox_raise_chain(ctx, prim, UnboxRaise::NullReturn);
}

/// Which refusal of `ValueConversions.unbox<W>(Object, boolean)` a native
/// `asType` unbox serves ([`unbox_raise_chain`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnboxRaise {
    /// A `null`: `primitiveConversion` returns it and `unbox<W>` calls the
    /// `Number` getter on it (an NPE raised in `unbox<W>`).
    NullReturn,
    /// A value whose class does not convert to the primitive (a wrapper that
    /// does not widen, a non-wrapper `Number`): `primitiveConversion`'s
    /// `wrap.wrapperType().cast(x)`, a `ClassCastException` raised in
    /// `Class.cast`. JIT round j27 wave 4, lane MHROWS2 (WMTE-1).
    Unconvertible,
    /// A value that is no `Number`, `Boolean` or `Character` (an array, a
    /// `String`): `primitiveConversion`'s `(Number) x` `checkcast` raises.
    NotANumber,
}

/// `ValueConversions.unbox<W>(Object, boolean)`'s name and descriptor for the
/// primitive descriptor `prim`, `None` for any other descriptor.
fn value_conversions_unbox(prim: &str) -> Option<(&'static str, &'static str)> {
    Some(match prim {
        "I" => ("unboxInteger", "(Ljava/lang/Object;Z)I"),
        "J" => ("unboxLong", "(Ljava/lang/Object;Z)J"),
        "Z" => ("unboxBoolean", "(Ljava/lang/Object;Z)Z"),
        "C" => ("unboxCharacter", "(Ljava/lang/Object;Z)C"),
        "B" => ("unboxByte", "(Ljava/lang/Object;Z)B"),
        "S" => ("unboxShort", "(Ljava/lang/Object;Z)S"),
        "F" => ("unboxFloat", "(Ljava/lang/Object;Z)F"),
        "D" => ("unboxDouble", "(Ljava/lang/Object;Z)D"),
        _ => return None,
    })
}

/// The JDK frames HotSpot lists above an `asType` unbox refusal of `raise`
/// for the primitive `prim`, OUTERMOST first, starting at the handle's
/// invoker the door served (its hidden `LambdaForm` frames are not listed):
/// a null raises in `unbox<W>` (`L3W60NullUnboxFrames`); an unconvertible
/// value raises in `Class.cast` under `unbox<W>` -> `primitiveConversion`,
/// the frames reflection already lists for the same conversion
/// (`stackwalker.rs` `argument_cast_frames`, measured on HotSpot 25:
/// `unboxInteger:81, primitiveConversion:252, Class.cast:3492`). Pure.
pub(crate) fn unbox_raise_chain(
    prim: &str,
    raise: UnboxRaise,
) -> Option<cratonvm_native_api::ServedRaiseChain> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const VALUE_CONVERSIONS: &str = "sun/invoke/util/ValueConversions";
    const PRIMITIVE_CONVERSION: &str =
        "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;";
    let (name, descriptor) = value_conversions_unbox(prim)?;
    let unbox = |site| ServedRaiseHop {
        class: VALUE_CONVERSIONS,
        name,
        descriptor,
        site,
    };
    let call_conversion = ServedRaiseHopSite::LastCall {
        name: "primitiveConversion",
        descriptor: Some(PRIMITIVE_CONVERSION),
    };
    let conversion = |site| ServedRaiseHop {
        class: VALUE_CONVERSIONS,
        name: "primitiveConversion",
        descriptor: PRIMITIVE_CONVERSION,
        site,
    };
    Some(match raise {
        UnboxRaise::NullReturn => ServedRaiseChain {
            throwable_class: "java/lang/NullPointerException",
            hops: vec![HIDDEN_INVOKER_HOP, unbox(ServedRaiseHopSite::Raise)],
        },
        UnboxRaise::Unconvertible => ServedRaiseChain {
            throwable_class: CLASS_CAST_EXCEPTION,
            hops: vec![
                HIDDEN_INVOKER_HOP,
                unbox(call_conversion),
                conversion(ServedRaiseHopSite::LastCall {
                    name: CLASS_CAST_RAISE_HOP.name,
                    descriptor: Some(CLASS_CAST_RAISE_HOP.descriptor),
                }),
                CLASS_CAST_RAISE_HOP,
            ],
        },
        UnboxRaise::NotANumber => ServedRaiseChain {
            throwable_class: CLASS_CAST_EXCEPTION,
            hops: vec![
                HIDDEN_INVOKER_HOP,
                unbox(call_conversion),
                conversion(ServedRaiseHopSite::LocalCheckcast {
                    class_name: "java/lang/Number",
                }),
            ],
        },
    })
}

/// The first hop of a chain raised under a handle's invoker the door served:
/// HotSpot runs the handle's hidden `LambdaForm` frames there (not listed).
const HIDDEN_INVOKER_HOP: cratonvm_native_api::ServedRaiseHop =
    cratonvm_native_api::ServedRaiseHop {
        class: "java/lang/invoke/MethodHandle",
        name: "invokeExact",
        descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
        site: cratonvm_native_api::ServedRaiseHopSite::Hidden,
    };

/// `Class.cast`'s one `new ClassCastException`, the innermost hop.
const CLASS_CAST_RAISE_HOP: cratonvm_native_api::ServedRaiseHop =
    cratonvm_native_api::ServedRaiseHop {
        class: "java/lang/Class",
        name: "cast",
        descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
        site: cratonvm_native_api::ServedRaiseHopSite::Raise,
    };

const CLASS_CAST_EXCEPTION: &str = "java/lang/ClassCastException";

/// The JDK frame HotSpot lists above an `asType` reference -> reference cast
/// the VM serves natively for a REAL handle: the adapter's `Class.cast`, right
/// above the caller (its `LambdaForm` frames are hidden), as reflection lists
/// it for the same conversion (`stackwalker.rs` `argument_cast_frames`,
/// `ReflectiveArgumentCast::Reference`, `L3W47ConstructorArgCause`). Pure.
/// JIT round j27 wave 4, lane MHROWS2 (WMTE-1).
pub(crate) fn reference_cast_raise_chain() -> cratonvm_native_api::ServedRaiseChain {
    cratonvm_native_api::ServedRaiseChain {
        throwable_class: CLASS_CAST_EXCEPTION,
        hops: vec![HIDDEN_INVOKER_HOP, CLASS_CAST_RAISE_HOP],
    }
}

/// Note [`reference_cast_raise_chain`] for the funnel's mark, for a door
/// serving a REAL handle's adapted call. Out of line, on the refusal path.
#[cold]
#[inline(never)]
pub(crate) fn note_reference_cast_chain(ctx: &mut dyn NativeContext) {
    let _ = ctx.note_served_raise_chain(reference_cast_raise_chain());
}

/// Note [`unbox_raise_chain`] for the funnel's mark. Out of line, on the
/// refusal path.
#[cold]
#[inline(never)]
fn note_unbox_raise_chain(ctx: &mut dyn NativeContext, prim: &str, raise: UnboxRaise) {
    if let Some(chain) = unbox_raise_chain(prim, raise) {
        let _ = ctx.note_served_raise_chain(chain);
    }
}

pub(crate) fn mh_unbox_for_primitive(
    ctx: &mut dyn NativeContext,
    prim: &str,
    value: Value,
    site_param: Option<&str>,
) -> Result<Value, MethodCallFailed> {
    let Some(wrapper) = wrapper_desc_for_primitive(prim) else {
        return Ok(value);
    };
    let obj = match value {
        Value::Object(Some(obj)) => obj,
        Value::Object(None) => {
            let prim_name = descriptor_simple_name(prim).unwrap_or_default();
            let message = if site_param == Some(wrapper) {
                format!(
                    "Cannot invoke \"{}.{prim_name}Value()\" because \"x\" is null",
                    mh_class_get_name(wrapper)
                )
            } else {
                // `unboxBoolean` / `unboxCharacter` read `intValue()` (they
                // mask / narrow it afterwards); the rest read their own width.
                let getter = match prim {
                    "B" => "byteValue",
                    "S" => "shortValue",
                    "J" => "longValue",
                    "F" => "floatValue",
                    "D" => "doubleValue",
                    _ => "intValue",
                };
                note_null_unbox_chain(ctx, prim);
                format!(
                    "Cannot invoke \"java.lang.Number.{getter}()\" because the return value of \
                     \"sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, \
                     Object, boolean)\" is null"
                )
            };
            return Err(RuntimeError::NullPointerException {
                message: Some(message),
            }
            .into());
        }
        other => return Ok(widen_primitive_to_descriptor(other, prim)),
    };
    let not_a_number = |from: String| -> MethodCallFailed {
        RuntimeError::ClassCastException {
            message: format!("{from} cannot be cast to java.lang.Number"),
        }
        .into()
    };
    if ctx.object_is_array(obj) {
        // Read before the note, which loads classes (a GC point).
        let shown = entry_value_type_display(&*ctx, obj);
        note_unbox_raise_chain(ctx, prim, UnboxRaise::NotANumber);
        return Err(not_a_number(shown));
    }
    let cid = ctx.class_id_of_object(obj);
    let Some(name) = ctx.class_name_of_id(cid) else {
        return Ok(widen_primitive_to_descriptor(
            crate::lang_class::unbox_value(&*ctx, obj),
            prim,
        ));
    };
    let value_desc = format!("L{name};");
    // `Class.cast` refuses a wrapper that does not widen and a non-wrapper
    // `Number`; anything else fails the `(Number) x` cast first.
    let class_cast = match primitive_desc_for_wrapper(&value_desc) {
        Some(src) if primitive_widens_to(src, prim) => {
            return Ok(widen_primitive_to_descriptor(
                crate::lang_class::unbox_value(&*ctx, obj),
                prim,
            ));
        }
        Some(_) => true,
        None => ctx.class_assignable_to_name(cid, "java/lang/Number") == Some(true),
    };
    if !class_cast {
        // HotSpot lists `unbox<W>` -> `primitiveConversion` (its `(Number) x`).
        note_unbox_raise_chain(ctx, prim, UnboxRaise::NotANumber);
        return Err(not_a_number(name.class_text(&*ctx)));
    }
    // WMTE-1 (j27 wave 4, lane MHROWS2): HotSpot lists `unbox<W>` ->
    // `primitiveConversion` -> `Class.cast` above the caller.
    note_unbox_raise_chain(ctx, prim, UnboxRaise::Unconvertible);
    Err(RuntimeError::ClassCastException {
        message: format!(
            "Cannot cast {} to {}",
            mh_class_get_name(&value_desc),
            mh_class_get_name(wrapper)
        ),
    }
    .into())
}

/// `spreadArrayChecks` (`MethodHandle`): `arrayType` must be an array type,
/// and `arrayLength` a legal length for it.
pub(crate) fn mh_spread_array_refusal(
    ctx: &dyn NativeContext,
    array_desc: &str,
    length: i32,
) -> Option<MethodCallFailed> {
    let Some(component) = array_desc.strip_prefix('[') else {
        return Some(mh_illegal_argument(format!(
            "not an array type: {}",
            mh_class_to_string(ctx, array_desc)
        )));
    };
    if (length & 0x7F) != length {
        if (length & 0xFF) != length {
            return Some(mh_illegal_argument(format!(
                "array length is not legal: {length}"
            )));
        }
        if matches!(component, "J" | "D") {
            return Some(mh_illegal_argument(format!(
                "array length is not legal for long[] or double[]: {length}"
            )));
        }
    }
    None
}

/// `param.isAssignableFrom(arrayType)` for a collector's trailing parameter,
/// or `None` when it cannot be told without guessing.
pub(crate) fn mh_array_assignable_to(ctx: &dyn NativeContext, array_desc: &str, param: &str) -> Option<bool> {
    if param == array_desc {
        return Some(true);
    }
    if matches!(
        param,
        "Ljava/lang/Object;" | "Ljava/lang/Cloneable;" | "Ljava/io/Serializable;"
    ) {
        return Some(true);
    }
    if is_primitive_descriptor(param) {
        return Some(false);
    }
    let (Some(pc), Some(ac)) = (param.strip_prefix('['), array_desc.strip_prefix('[')) else {
        // A non-array reference parameter other than the three above: no
        // array type is assignable to a class or another interface.
        return Some(false);
    };
    if is_primitive_descriptor(pc) || is_primitive_descriptor(ac) {
        return Some(pc == ac);
    }
    if pc.starts_with('[') || ac.starts_with('[') {
        return mh_array_assignable_to(ctx, ac, pc);
    }
    // Two reference components: `ac`'s class assignable to `pc`'s.
    let a_internal = ac.strip_prefix('L')?.strip_suffix(';')?;
    let p_internal = pc.strip_prefix('L')?.strip_suffix(';')?;
    let a_id = ctx.class_id_by_name(a_internal)?;
    ctx.class_assignable_to_name(a_id, p_internal)
}

/// `asCollectorChecks` (`MethodHandle`) for a collector at `pos` of a handle
/// of type `desc`; the `this` of its message is `MethodHandle<type>`.
pub(crate) fn mh_collector_refusal(
    ctx: &dyn NativeContext,
    desc: &str,
    array_desc: &str,
    pos: i64,
    length: i32,
) -> Option<MethodCallFailed> {
    if let Some(refusal) = mh_spread_array_refusal(ctx, array_desc, length) {
        return Some(refusal);
    }
    let (params, _) = split_descriptor_params(desc)?;
    if pos < 0 || pos >= params.len() as i64 {
        return Some(mh_illegal_argument("bad collect position".to_string()));
    }
    let param = &params[pos as usize];
    if !mh_array_assignable_to(ctx, array_desc, param)? {
        return Some(mh_illegal_argument(format!(
            "array type not assignable to argument: MethodHandle{}, {}",
            method_type_display_in(&*ctx, desc)?,
            mh_class_to_string(ctx, array_desc)
        )));
    }
    // Interpreter round i7 wave 141, lane L4 (`CRATONVM_MH_COLLECTOR_SLOT_CHECKS`).
    if !mh_collector_slot_checks_enabled() {
        return None;
    }
    mh_collector_slot_refusal(ctx, &params, pos as usize, array_desc, length)
}

/// `CRATONVM_MH_COLLECTOR_SLOT_CHECKS` (parity, default ON, `=0` off;
/// interpreter round i7 wave 141, lane L4, `L4W141SlotLimits`): the
/// collector natives make JDK 25's two slot-count refusals after
/// `asCollectorChecks` ([`mh_collector_slot_refusal`]). Read on a factory call
/// only.
pub(crate) fn mh_collector_slot_checks_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_COLLECTOR_SLOT_CHECKS")
}

/// What JDK 25's `MethodHandle.asCollector(pos, arrayType, n)` refuses after
/// `asCollectorChecks` passed (interpreter round i7 wave 141, lane L4; probe
/// `L4W141SlotLimits`, measured on HotSpot 25.0.3):
///
/// 1. `type().asCollectorType(arrayType, pos, n)`: the new type's parameter
///    slots past 255 (`MethodType.checkSlotCount`), `IllegalArgumentException:
///    bad parameter count <slots>`;
/// 2. `MethodHandleImpl.varargsArray(arrayType, n)`: for `n >= 126`, the
///    collector's slots (`n`, twice that for `long` / `double`) past 254 (one
///    is the receiver handle's), `too many arguments: <arrayType simple name>,
///    length <n>` (`asCollector(Object[].class, 255)` on an `(Object[])` handle).
///
/// The natives answered a collector for both. `params` are the target's,
/// `pos` the collected one (already checked in range).
pub(crate) fn mh_collector_slot_refusal(
    ctx: &dyn NativeContext,
    params: &[String],
    pos: usize,
    array_desc: &str,
    length: i32,
) -> Option<MethodCallFailed> {
    let slots_of = |p: &str| if matches!(p, "J" | "D") { 2usize } else { 1 };
    let component = array_desc.strip_prefix('[')?;
    let n = usize::try_from(length).ok()?;
    let kept: usize = params
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != pos)
        .map(|(_, p)| slots_of(p.as_str()))
        .sum();
    let slots = kept + n * slots_of(component);
    if slots > 255 {
        return Some(mh_illegal_argument(format!("bad parameter count {slots}")));
    }
    if n >= 126 {
        let collector_slots = if n <= 254 { n * slots_of(component) } else { n };
        if collector_slots > 254 {
            let simple = method_type_display_in(ctx, &format!("(){array_desc}"))?;
            return Some(mh_illegal_argument(format!(
                "too many arguments: {}, length {n}",
                simple.strip_prefix("()").unwrap_or(&simple)
            )));
        }
    }
    None
}

/// `MethodHandles.tryFinallyChecks` for a target of type `tdesc` and a
/// cleanup of type `cdesc`, with its `misMatchedTypes` messages
/// (`<what> must match: <t1> != <t2>`).
/// JDK 25 `MethodHandles.whileLoopChecks(init, pred, body)`, shared by
/// `whileLoop` and `doWhileLoop` (interpreter round i1 wave 44, lane L4;
/// `L4W44CombinatorHunt` row `whileLoop-bad-pred`): `pred` / `body` null is
/// `Objects.requireNonNull`'s NPE; a non-`void` body whose leading parameter
/// is not its return type, a predicate that does not return `boolean` over
/// a prefix of the body's parameters, and an `init` that does not return the
/// body's type over a prefix of the body's parameters after `V` are
/// `misMatchedTypes`' `IllegalArgumentException` (`<what> must match: <t1> !=
/// <t2>`). `None` when every check passes or a type cannot be read (an
/// accept, as before).
pub(crate) fn mh_while_loop_refusal(
    ctx: &mut dyn NativeContext,
    init: Value,
    pred: Value,
    body: Value,
) -> Option<MethodCallFailed> {
    let (Value::Object(Some(pred)), Value::Object(Some(body))) = (pred, body) else {
        return Some(mh_factory_null_argument(None));
    };
    let body_desc = mh_type_descriptor(ctx, body)?;
    let (inner, ret) = split_descriptor_params(&body_desc)?;
    let mismatch = |what: &str, t1: String, t2: String| {
        Some(mh_illegal_argument(format!(
            "{what} must match: {t1} != {t2}"
        )))
    };
    let outer: &[String] = if ret == "V" {
        &inner
    } else if inner.first().map(String::as_str) != Some(ret.as_str()) {
        let mut expected = Vec::with_capacity(inner.len() + 1);
        expected.push(ret.clone());
        expected.extend(inner.iter().cloned());
        return mismatch(
            "body function",
            loop_type_text(&*ctx, &inner, &ret)?,
            loop_type_text(&*ctx, &expected, &ret)?,
        );
    } else {
        &inner[1..]
    };
    // `effectivelyIdenticalParameters(0, list)`: a prefix of `list`.
    let prefix_of = |params: &[String], list: &[String]| {
        params.len() <= list.len() && params.iter().zip(list).all(|(a, b)| a == b)
    };
    let pred_desc = mh_type_descriptor(ctx, pred)?;
    let (pred_params, pred_ret) = split_descriptor_params(&pred_desc)?;
    if pred_ret != "Z" || !prefix_of(&pred_params, &inner) {
        return mismatch(
            "loop predicate",
            loop_type_text(&*ctx, &pred_params, &pred_ret)?,
            loop_type_text(&*ctx, &inner, "Z")?,
        );
    }
    if let Value::Object(Some(init)) = init {
        let init_desc = mh_type_descriptor(ctx, init)?;
        let (init_params, init_ret) = split_descriptor_params(&init_desc)?;
        if init_ret != ret || !prefix_of(&init_params, outer) {
            return mismatch(
                "loop initializer",
                loop_type_text(&*ctx, &init_params, &init_ret)?,
                loop_type_text(&*ctx, outer, &ret)?,
            );
        }
    }
    None
}

/// `CRATONVM_MH_ITERATED_LOOP_CHECKS` (parity, default ON, `=0` off;
/// interpreter round i7 wave 141, lane L4, `L4W141IteratedLoopChecks`): the
/// `iteratedLoop` bridge makes JDK 25's `iteratedLoopChecks` refusals
/// ([`mh_iterated_loop_refusal`]) and its loop raises the JDK's helpful NPE for
/// a null `Iterable`. `=0`: no refusal (a handle for any shape) and the old
/// `iteratedLoop: iterable is null` text. Read on a factory call and on a null
/// iterable only.
pub(crate) fn mh_iterated_loop_checks_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_ITERATED_LOOP_CHECKS")
}

/// Is the class named by the reference descriptor `desc` `target` or a subtype
/// of it (`target.isAssignableFrom(desc's class)`)? `Some(false)` for a
/// primitive, `void` or an array; `None` when the class cannot be named here
/// (no refusal may rest on a guess).
pub(crate) fn mh_desc_assignable_to(ctx: &dyn NativeContext, desc: &str, target: &str) -> Option<bool> {
    let Some(internal) = desc.strip_prefix('L').and_then(|d| d.strip_suffix(';')) else {
        return Some(false);
    };
    if internal == target {
        return Some(true);
    }
    let cid = ctx.class_id_by_name(internal)?;
    ctx.class_assignable_to_name(cid, target)
}

/// JDK 25 `MethodHandles.iteratedLoopChecks(iterator, init, body)` over the
/// handles' type descriptors (interpreter round i7 wave 141, lane L4; probe
/// `L4W141IteratedLoopChecks`, measured on HotSpot 25.0.3): the `body
/// function` / `iterator parameters` / `loop initializer` mismatches
/// (`<what> must match: <t1> != <t2>`), `iteratedLoop first argument must have
/// Iterator return type`, and `inferred first loop argument must inherit from
/// Iterable: <class>`. The bridge answered a handle for every shape. `None`:
/// no refusal, or a type that could not be read (an accept, as before).
pub(crate) fn mh_iterated_loop_refusal(
    ctx: &mut dyn NativeContext,
    iterator: Value,
    init: Value,
    body: ObjectRef,
) -> Option<MethodCallFailed> {
    let body_desc = mh_type_descriptor(ctx, body)?;
    let (inner, ret) = split_descriptor_params(&body_desc)?;
    let mismatch = |what: &str, t1: String, t2: String| {
        Some(mh_illegal_argument(format!("{what} must match: {t1} != {t2}")))
    };
    let vsize = usize::from(ret != "V");
    if vsize != 0 && inner.first() != Some(&ret) {
        let mut expected = Vec::with_capacity(inner.len() + 1);
        expected.push(ret.clone());
        expected.extend(inner.iter().cloned());
        return mismatch(
            "body function",
            loop_type_text(&*ctx, &inner, &ret)?,
            loop_type_text(&*ctx, &expected, &ret)?,
        );
    } else if inner.len() <= vsize {
        let mut expected = inner.clone();
        expected.insert(vsize, DESC_OBJECT.to_string());
        return mismatch(
            "body function",
            loop_type_text(&*ctx, &inner, &ret)?,
            loop_type_text(&*ctx, &expected, &ret)?,
        );
    }
    let prefix_of = |params: &[String], list: &[String]| {
        params.len() <= list.len() && params.iter().zip(list).all(|(a, b)| a == b)
    };
    let mut external: Vec<String> = inner[vsize + 1..].to_vec();
    match iterator {
        Value::Object(Some(it)) => {
            let it_desc = mh_type_descriptor(ctx, it)?;
            let (it_params, it_ret) = split_descriptor_params(&it_desc)?;
            if external.is_empty() {
                external = it_params.clone();
            }
            if !mh_desc_assignable_to(&*ctx, &it_ret, "java/util/Iterator")? {
                return Some(mh_illegal_argument(
                    "iteratedLoop first argument must have Iterator return type".to_string(),
                ));
            }
            if !prefix_of(&it_params, &external) {
                return mismatch(
                    "iterator parameters",
                    loop_type_text(&*ctx, &it_params, &it_ret)?,
                    loop_type_text(&*ctx, &external, &it_ret)?,
                );
            }
        }
        _ => {
            if external.is_empty() {
                external = vec!["Ljava/lang/Iterable;".to_string()];
            } else if !mh_desc_assignable_to(&*ctx, &external[0], "java/lang/Iterable")? {
                return Some(mh_illegal_argument(format!(
                    "inferred first loop argument must inherit from Iterable: {}",
                    mh_class_to_string(&*ctx, &external[0])
                )));
            }
        }
    }
    if let Value::Object(Some(init)) = init {
        let init_desc = mh_type_descriptor(ctx, init)?;
        let (init_params, init_ret) = split_descriptor_params(&init_desc)?;
        if init_ret != ret || !prefix_of(&init_params, &external) {
            return mismatch(
                "loop initializer",
                loop_type_text(&*ctx, &init_params, &init_ret)?,
                loop_type_text(&*ctx, &external, &ret)?,
            );
        }
    }
    None
}

/// JDK 25 `MethodHandles.countedLoopChecks(start, end, init, body)` over the
/// handles' type descriptors (interpreter round i1 wave 44, lane L4;
/// `L4W44CombinatorHunt` row `countedLoop-bad-iterations`). The
/// three-handle `countedLoop(iterations, init, body)` is
/// `countedLoop(empty(iterations.type()), iterations, init, body)`, so its
/// `start_desc` is the iterations handle's own. Also answers the loop's
/// external parameter list (`outerList`, which is the `end` handle's when the
/// body declares nothing after `(V int)`). `Err` is the refusal, `Ok(None)`
/// a type that could not be read (an accept, as before).
pub(crate) fn mh_counted_loop_checks(
    ctx: &dyn NativeContext,
    start_desc: &str,
    end_desc: &str,
    init_desc: Option<&str>,
    body_desc: &str,
) -> Result<Option<Vec<String>>, MethodCallFailed> {
    let mismatch = |what: &str, t1: Option<String>, t2: Option<String>| -> MethodCallFailed {
        mh_illegal_argument(format!(
            "{what} must match: {} != {}",
            t1.unwrap_or_default(),
            t2.unwrap_or_default()
        ))
    };
    let (Some((sp, sr)), Some((ep, er)), Some((inner, ret))) = (
        split_descriptor_params(start_desc),
        split_descriptor_params(end_desc),
        split_descriptor_params(body_desc),
    ) else {
        return Ok(None);
    };
    if sr != "I" {
        return Err(mismatch(
            "start function",
            loop_type_text(ctx, &sp, &sr),
            loop_type_text(ctx, &sp, "I"),
        ));
    }
    if er != "I" {
        return Err(mismatch(
            "end function",
            loop_type_text(ctx, &ep, &er),
            loop_type_text(ctx, &ep, "I"),
        ));
    }
    let vsize = usize::from(ret != "V");
    if vsize == 1 && inner.first().map(String::as_str) != Some(ret.as_str()) {
        let mut expected = Vec::with_capacity(inner.len() + 1);
        expected.push(ret.clone());
        expected.extend(inner.iter().cloned());
        return Err(mismatch(
            "body function",
            loop_type_text(ctx, &inner, &ret),
            loop_type_text(ctx, &expected, &ret),
        ));
    }
    if inner.len() <= vsize || inner[vsize] != "I" {
        let mut expected = inner.clone();
        expected.insert(vsize.min(expected.len()), "I".to_string());
        return Err(mismatch(
            "body function",
            loop_type_text(ctx, &inner, &ret),
            loop_type_text(ctx, &expected, &ret),
        ));
    }
    let mut outer: Vec<String> = inner[vsize + 1..].to_vec();
    if outer.is_empty() {
        outer = ep.clone();
    }
    let prefix_of = |params: &[String], list: &[String]| {
        params.len() <= list.len() && params.iter().zip(list).all(|(a, b)| a == b)
    };
    if !prefix_of(&sp, &outer) {
        return Err(mismatch(
            "start parameter types",
            loop_type_text(ctx, &sp, &sr),
            loop_type_text(ctx, &outer, "I"),
        ));
    }
    if end_desc != start_desc && !prefix_of(&ep, &outer) {
        return Err(mismatch(
            "end parameter types",
            loop_type_text(ctx, &ep, &er),
            loop_type_text(ctx, &outer, "I"),
        ));
    }
    if let Some(init_desc) = init_desc {
        let Some((ip, ir)) = split_descriptor_params(init_desc) else {
            return Ok(Some(outer));
        };
        if ir != ret || !prefix_of(&ip, &outer) {
            return Err(mismatch(
                "loop initializer",
                loop_type_text(ctx, &ip, &ir),
                loop_type_text(ctx, &outer, &ret),
            ));
        }
    }
    Ok(Some(outer))
}

/// [`mh_counted_loop_checks`] over the handles themselves, answering the
/// loop's type descriptor `(outerList)V` (`V` the body's return), or the
/// body-derived [`make_loop_type_desc`] when a type cannot be read.
pub(crate) fn counted_loop_type_desc(
    ctx: &mut dyn NativeContext,
    start: ObjectRef,
    end: ObjectRef,
    init: Value,
    body: ObjectRef,
) -> Result<String, MethodCallFailed> {
    let start_desc = mh_type_descriptor(ctx, start);
    let end_desc = mh_type_descriptor(ctx, end);
    let body_desc = mh_type_descriptor(ctx, body);
    let init_desc = match init {
        Value::Object(Some(i)) => mh_type_descriptor(ctx, i),
        _ => None,
    };
    if let (Some(s), Some(e), Some(b)) = (&start_desc, &end_desc, &body_desc) {
        if let Some(outer) = mh_counted_loop_checks(&*ctx, s, e, init_desc.as_deref(), b)? {
            return Ok(format!("({}){}", outer.concat(), return_type_desc(b)));
        }
    }
    Ok(make_loop_type_desc(ctx, body, 2, None))
}

/// `MethodType.toString()` of `(params)ret` for [`mh_while_loop_refusal`].
pub(crate) fn loop_type_text(ctx: &dyn NativeContext, params: &[String], ret: &str) -> Option<String> {
    method_type_display_in(ctx, &format!("({}){ret}", params.concat()))
}

pub(crate) fn mh_try_finally_refusal(
    ctx: &dyn NativeContext,
    tdesc: &str,
    cdesc: &str,
) -> Option<MethodCallFailed> {
    let (tparams, rt) = split_descriptor_params(tdesc)?;
    let (cparams, crt) = split_descriptor_params(cdesc)?;
    let mismatch = |what: &str, t1: String, t2: String| {
        Some(mh_illegal_argument(format!(
            "{what} must match: {t1} != {t2}"
        )))
    };
    if rt != crt {
        return mismatch(
            "target and return types",
            mh_class_to_string(ctx, &crt),
            mh_class_to_string(ctx, &rt),
        );
    }
    // `Throwable.class.isAssignableFrom(cleanup.parameterType(0))`; a
    // cleanup with no parameter fails differently on HotSpot: `parameterType`'s
    // index error (interpreter round i7 wave 141, lane L4,
    // `CRATONVM_MH_TRY_FINALLY_CLEANUP_SHAPES`; it was left alone).
    let shapes = mh_try_finally_cleanup_shapes_enabled();
    let Some(first) = cparams.first() else {
        return shapes.then(|| mh_parameter_type_index_error(0, 0));
    };
    let throwable = if is_primitive_descriptor(first) || first.starts_with('[') {
        false
    } else {
        let internal = first.strip_prefix('L')?.strip_suffix(';')?;
        let cid = ctx.class_id_by_name(internal)?;
        ctx.class_assignable_to_name(cid, "java/lang/Throwable")?
    };
    if !throwable {
        return mismatch(
            "cleanup first argument and Throwable",
            method_type_display_in(&*ctx, cdesc)?,
            "class java.lang.Throwable".to_string(),
        );
    }
    if rt != "V" && cparams.get(1).map(String::as_str) != Some(rt.as_str()) {
        if cparams.len() < 2 {
            return shapes.then(|| mh_parameter_type_index_error(1, cparams.len()));
        }
        return mismatch(
            "cleanup second argument and target return type",
            method_type_display_in(&*ctx, cdesc)?,
            mh_class_to_string(ctx, &rt),
        );
    }
    // `effectivelyIdenticalParameters`: the cleanup's parameters after
    // (Throwable[, result]) are a prefix of the target's.
    let skip = if rt == "V" { 1 } else { 2 };
    let rest = cparams.get(skip..).unwrap_or(&[]);
    let prefix_ok = cparams.len() >= skip
        && rest.len() <= tparams.len()
        && rest.iter().zip(tparams.iter()).all(|(a, b)| a == b);
    if !prefix_ok {
        // JDK 25 names the target's TYPE (`misMatchedTypes(.., cleanup.type(),
        // target.type())`, measured `!= (int,String)int`); wave 141 (lane L4,
        // `CRATONVM_MH_TRY_FINALLY_CLEANUP_SHAPES`). `=0`: the old list text.
        let t2 = if shapes {
            method_type_display_in(&*ctx, tdesc)?
        } else {
            let list: Vec<String> = tparams.iter().map(|p| mh_class_to_string(ctx, p)).collect();
            format!("[{}]", list.join(", "))
        };
        return mismatch(
            "cleanup parameters after (Throwable,result) and target parameter list prefix",
            method_type_display_in(&*ctx, cdesc)?,
            t2,
        );
    }
    None
}

/// `CRATONVM_MH_TRY_FINALLY_CLEANUP_SHAPES` (parity, default ON, `=0` off;
/// interpreter round i7 wave 141, lane L4, `L4W141TryFinallyCleanup`): the
/// `tryFinally` bridge answers JDK 25's `tryFinallyChecks` for a cleanup too
/// short to index (`parameterType`'s `ArrayIndexOutOfBoundsException`), names
/// the target's type in the prefix mismatch, and its dispatch hands the
/// cleanup only the arguments it declares (`(Throwable[, result], A...)`, a
/// prefix of the target's: the JDK adapts it with `dropArgumentsToMatch`).
/// Read on a factory call and per `tryFinally` dispatch, like the other
/// combinator switches of this file.
pub(crate) fn mh_try_finally_cleanup_shapes_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_TRY_FINALLY_CLEANUP_SHAPES")
    })
}

/// `MethodType.parameterType(index)` on a type of `len` parameters:
/// `ArrayIndexOutOfBoundsException: Index <index> out of bounds for length <len>`.
pub(crate) fn mh_parameter_type_index_error(index: usize, len: usize) -> MethodCallFailed {
    let index = i32::try_from(index).unwrap_or(i32::MAX);
    MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
        RuntimeError::ArrayIndexOutOfBoundsException {
            index,
            message: Some(format!("Index {index} out of bounds for length {len}")),
        },
    ))
}

pub(crate) fn register_array_element_accessor_bridges(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mh = "java/lang/invoke/MethodHandles";
    r.register(
        mh,
        "arrayElementGetter",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| array_element_accessor_handle(ctx, args, false),
    );
    r.register(
        mh,
        "arrayElementSetter",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| array_element_accessor_handle(ctx, args, true),
    );
    r.register(
        mh,
        "arrayLength",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let arr_desc: String = match args.first() {
                Some(Value::Object(Some(mirror))) => {
                    match resolve_class_name_robust(ctx, *mirror) {
                        Some(name) => class_name_to_descriptor(&name).into_owned(),
                        None => format!("[{DESC_OBJECT}"),
                    }
                }
                // `arrayLength(null)` (wave 40).
                Some(Value::Object(None)) => return Err(mh_array_class_null()),
                _ => format!("[{DESC_OBJECT}"),
            };
            // `MethodHandles.arrayLength`: "not an array: int" (wave 23).
            if !arr_desc.starts_with('[') {
                return Err(mh_illegal_argument(format!(
                    "not an array: {}",
                    mh_class_to_string(&*ctx, &arr_desc)
                )));
            }
            let desc = format!("({arr_desc})I");
            let handle =
                alloc_method_handle(ctx, &arr_desc, "arrayLength", &desc, MH_KIND_ARRAY_LENGTH)?;
            Ok(Some(Value::Object(Some(handle))))
        },
    );
    r.set_category(__prev_cat);
}

/// The `NullPointerException` a `MethodHandles` factory bridge raises for a
/// Java `null` class or type argument, as the JDK body it stands for does
/// (interpreter round i1 wave 40, lane L4; page
/// `interpreter-L4-methodhandles-factories-answer-a-handle-for-a-null-class-FIXED-20261004.md`). The
/// bridges answered a handle of a default type.
pub(crate) fn mh_factory_null_argument(message: Option<&str>) -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: message.map(str::to_string),
    }
    .into()
}

/// The `NullPointerException` `MethodHandles.insertArguments(target, pos,
/// values)` raises for a null `values` or `target`, in the JDK body's order
/// (`int insCount = values.length;` first, then `insertArgumentsChecks`'
/// `target.type()`), or `None` (round 15 wave 6, lane mh5). `args` are the
/// bridge's: `[target, pos, values]`.
pub(crate) fn insert_arguments_null_refusal(args: &[Value]) -> Option<MethodCallFailed> {
    if matches!(args.get(2), Some(Value::Object(None))) {
        return Some(mh_factory_null_argument(Some(
            "Cannot read the array length because \"values\" is null",
        )));
    }
    if matches!(args.first(), Some(Value::Object(None))) {
        return Some(mh_factory_null_argument(Some(
            "Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"target\" is null",
        )));
    }
    None
}

/// `CRATONVM_MH_INSERT_ARGUMENTS_NULL_NPE` -- default ON (round 15 wave 6,
/// lane mh5); `0`/`false`/`off`/`no` restores the old answers to a null
/// argument of `insertArguments` (an INSERT adapter for a null `values`,
/// `null` for a null `target`). Both modes. Read only when one is null.
pub(crate) fn mh_insert_arguments_null_npe_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INSERT_ARGUMENTS_NULL_NPE")
}

/// [`mh_factory_null_argument`] for the three array factories
/// (`MethodHandleImpl.makeArrayElementAccessor`'s `arrayClass.isArray()`).
pub(crate) fn mh_array_class_null() -> MethodCallFailed {
    mh_factory_null_argument(Some(
        "Cannot invoke \"java.lang.Class.isArray()\" because \"arrayClass\" is null",
    ))
}

/// Shared body of the `arrayElementGetter` / `arrayElementSetter` bridges.
///
/// Mints a FUNCTIONAL `MH_KIND_ARRAY_GET` / `MH_KIND_ARRAY_SET` handle whose
/// `MH_DESC` (and therefore `type()`) is the exact JDK-contract accessor
/// signature — `(T[],int)T` for the getter, `(T[],int,T)void` for the setter —
/// derived from the supplied array `Class` mirror. See `MH_KIND_ARRAY_GET`'s
/// doc comment for the inert-handle defect this replaces.
///
/// A mirror we cannot name at all degrades to `[Ljava/lang/Object;` rather
/// than throwing: `ObjectStreamClass$RecordSupport.<clinit>` only needs a
/// non-null handle in its `PRIM_VALUE_EXTRACTORS` map, and a hard failure
/// there aborts EVERY record (de)serialization with a bogus
/// `no class def found: ObjectStreamClass$RecordSupport`. A mirror we CAN
/// name but which is not an array type is the genuine JDK
/// `IllegalArgumentException` case and is thrown as such.
pub(crate) fn array_element_accessor_handle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    setter: bool,
) -> MethodCallResult {
    let factory = if setter {
        "arrayElementSetter"
    } else {
        "arrayElementGetter"
    };
    let arr_desc: String = match args.first() {
        Some(Value::Object(Some(mirror))) => match resolve_class_name_robust(ctx, *mirror) {
            Some(name) => {
                let d = class_name_to_descriptor(&name).into_owned();
                if !d.starts_with('[') {
                    // HotSpot's wording (`MethodHandleImpl.makeArrayElementAccessor`
                    // via `newIllegalArgumentException("not an array", arrayClass)`):
                    // "not an array: class java.lang.String" (wave 23; it
                    // read "MethodHandles.arrayElementGetter: not an array
                    // type: java/lang/String").
                    return Err(mh_illegal_argument(format!(
                        "not an array: {}",
                        mh_class_to_string(&*ctx, &d)
                    )));
                }
                d
            }
            None => format!("[{DESC_OBJECT}"),
        },
        // A null `arrayClass`: HotSpot's helpful NPE (wave 40, lane L4). It
        // answered an `Object[]` accessor so that `method_handles_factories_p65`
        // (`vm/src/vm/tests.rs`) could unwrap a handle; `RecordSupport.<clinit>`
        // passes real mirrors (`byte[].class`, `Object[].class`).
        Some(Value::Object(None)) => return Err(mh_array_class_null()),
        // No argument at all (not reachable from Java): the erased accessor.
        _ => format!("[{DESC_OBJECT}"),
    };
    // Component descriptor: one `[` stripped off the array descriptor.
    let comp = arr_desc[1..].to_string();
    let (desc, kind) = if setter {
        (format!("({arr_desc}I{comp})V"), MH_KIND_ARRAY_SET)
    } else {
        (format!("({arr_desc}I){comp}"), MH_KIND_ARRAY_GET)
    };
    let handle = alloc_method_handle(ctx, &arr_desc, factory, &desc, kind)?;
    Ok(Some(Value::Object(Some(handle))))
}

/// `MethodHandles.constant(Class type, Object value)` — a *functional*
/// shim that returns an `MH_KIND_CONSTANT` handle which, when invoked,
/// yields the captured `value`.
///
/// Promoted to the real-JDK essentials path (see `register_builtins`) — and
/// allow-listed in `vm/src/vm/vm_exec.rs`'s `check_override` gate — because
/// the genuine JDK `MethodHandles.constant` bytecode runs the runtime
/// `BoundMethodHandle` *species* generator (`ClassSpecializer`), which the
/// VM's `MH_KIND_*` shim model does not implement. That path NPEs at
/// `ClassSpecializer.generateConcreteSpeciesCode` (the generated species
/// class came back without working species-data linkage), wrapped as
/// `ExceptionInInitializerError` for `BoundMethodHandle`. `SwitchPoint.<clinit>`
/// builds two constant handles (`K_true`/`K_false`), and Apache Groovy's
/// `IndyInterface.<clinit>` initializes a `SwitchPoint` before any script
/// runs — so without this shim every Groovy `invokedynamic` site is dead.
/// This mirrors the existing `register_array_element_accessor_bridges`
/// pattern (another concrete `MethodHandles` static factory whose JDK
/// bytecode CratonVM cannot execute).
pub(crate) fn register_method_handles_constant_bridge(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "java/lang/invoke/MethodHandles",
        "constant",
        "(Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // arg0 = return type (Class mirror); arg1 = the (boxed) value.
            let ret_desc = match args.first() {
                Some(Value::Object(Some(m))) => mirror_to_descriptor(ctx, *m).into_owned(),
                // `constant(null, x)`: HotSpot's message-less NPE (wave 40).
                Some(Value::Object(None)) => return Err(mh_factory_null_argument(None)),
                _ => DESC_OBJECT.to_string(),
            };
            let desc = format!("(){ret_desc}");
            let value = args.get(1).copied().unwrap_or(Value::Object(None));
            // `constant(void.class, x)` is refused before anything else
            // (wave 23; it minted a `()void` handle).
            if ret_desc == "V" {
                return Err(mh_illegal_argument("void type".to_string()));
            }
            // A primitive return type cannot hold `null` — javadoc: "if the
            // type is a primitive and the value is null a NullPointerException
            // is thrown". Measured (`k.constantNullPrimitive`,
            // `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`): CratonVM
            // accepted `constant(int.class, null)` where HotSpot 25 refuses at
            // this call. Only a genuine `null` (not merely an unreadable
            // value) trips this — anything else is an accept. Since wave 23
            // also a value `Wrapper.convert` / `Class.cast` refuses
            // (`constant(int.class, 5L)`, `constant(String.class, 5)`), with
            // HotSpot's messages (`mh_bind_value_refusal`).
            if let Some(refusal) = mh_bind_value_refusal(ctx, &ret_desc, value) {
                return Err(refusal);
            }
            // A wrapper that WIDENS to the primitive return type is stored
            // converted (`constant(long.class, 5)` answers `5L`): the
            // dispatch arm unboxes the bound value as it is, and an `Int`
            // where the handle's type says `long` is not what HotSpot's
            // `Wrapper.convert` hands back. The exact wrapper stays boxed,
            // as before.
            let value = match value {
                Value::Object(Some(obj))
                    if is_primitive_descriptor(&ret_desc)
                        && ctx.heap_kind_of(obj) == ObjectKind::Object =>
                {
                    let raw = crate::lang_class::unbox_value(&*ctx, obj);
                    let widened = widen_primitive_to_descriptor(raw, &ret_desc);
                    if std::mem::discriminant(&widened) == std::mem::discriminant(&raw) {
                        value
                    } else {
                        widened
                    }
                }
                other => other,
            };
            // JIT round 18 wave 9, lane mhloader: as `identity` (round 17
            // wave 8, lane mh7), the handle's `type` names the argument's OWN
            // class for a reference type a user-defined loader defines, not
            // the first-loaded class of that name. The value is pinned: the
            // type's build can load.
            let near = match args.first() {
                Some(Value::Object(Some(m)))
                    if ret_desc.starts_with('L') || ret_desc.starts_with('[') =>
                {
                    mirror_class_id(&*ctx, *m)
                        .map(|id| mh_element_class_id(&*ctx, id))
                        .filter(|&id| ctx.loader_id_of_class(id) >= 3)
                        .filter(|_| crate::lang_class::class_inner_classes_check_enabled())
                }
                _ => None,
            };
            let value_pin = match value {
                Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
                _ => None,
            };
            let handle =
                alloc_method_handle_with_receiver(ctx, "", "", &desc, MH_KIND_CONSTANT, None, near);
            let value = match value_pin {
                Some((pin, obj)) => {
                    let obj = ctx.read_native_pin(pin, obj);
                    ctx.unpin_native_roots(pin);
                    Value::Object(Some(obj))
                }
                None => value,
            };
            let handle = handle?;
            ctx.set_field(handle, MH_BOUND, value);
            Ok(Some(Value::Object(Some(handle))))
        },
    );
    // `throwException` (wave 48): a `Bridge`, stated here by the scope above.
    // A static invoke of a registered bridge reaches it through
    // `resolve_native_dispatch_wave1` in both modes; nothing is added to the
    // `check_override` allow-list.
    r.register(
        "java/lang/invoke/MethodHandles",
        "throwException",
        "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        make_throw_exception_handle,
    );
    r.set_category(__prev_cat);
}

/// `MethodHandles.throwException(Class rtype, Class exType)` (interpreter
/// round i1 wave 48, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`). JDK 25:
///
/// ```java
/// if (!Throwable.class.isAssignableFrom(exType))
///     throw new ClassCastException(exType.getName());
/// return MethodHandleImpl.throwException(methodType(returnType, exType));
/// ```
///
/// whose `LambdaForm` reads the `MethodType`'s `form`, which this model's
/// `MethodType` does not carry (every call was an NPE on `this.form`). The
/// answer is an `MH_KIND_THROW` handle of type `(exType)rtype`. The refusals
/// in the JDK's order, with HotSpot 25's texts (measured, probe
/// `L4W48ThrowException`): a `null` `exType` is `isAssignableFrom`'s
/// message-less NPE, a non-`Throwable` one the `ClassCastException` naming
/// it, a `null` `rtype` the NPE of `MethodType`'s intern (`hashCode` of
/// `this.rtype`).
pub(crate) fn make_throw_exception_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ex_mirror = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => return Err(mh_factory_null_argument(None)),
    };
    let throwable = ctx.class_id_by_name("java/lang/Throwable");
    let is_throwable = match (ctx.class_id_from_mirror(ex_mirror), throwable) {
        (Some(cid), Some(t)) => {
            !crate::lang_class::mirror_is_primitive(&*ctx, ex_mirror)
                && !crate::lang_class::mirror_is_array(&*ctx, ex_mirror)
                && (cid == t || ctx.is_subclass(cid, t))
        }
        // Cannot tell: accept, as every other unreadable case in this file.
        _ => !crate::lang_class::mirror_is_primitive(&*ctx, ex_mirror),
    };
    if !is_throwable {
        let name = crate::lang_class::mirror_class_name(&*ctx, ex_mirror)
            .map(|n| n.class_text(&*ctx))
            .unwrap_or_else(|| "?".to_string());
        return Err(RuntimeError::ClassCastException { message: name }.into());
    }
    let ret_desc = match args.first() {
        Some(Value::Object(Some(m))) => mirror_to_descriptor(ctx, *m).into_owned(),
        _ => {
            return Err(mh_factory_null_argument(Some(
                "Cannot invoke \"Object.hashCode()\" because \"this.rtype\" is null",
            )))
        }
    };
    let ex_desc = mirror_to_descriptor(ctx, ex_mirror).into_owned();
    let desc = format!("({ex_desc}){ret_desc}");
    // JIT round 18 wave 11 (lane mhresid;
    // `r18-mh-throwexception-type-loader-blind-FIXED-20261006.md`): the descriptor
    // string's names are resolved loader-blind (the first-loaded class of
    // each name), where HotSpot's `type()` holds the two mirrors it was given.
    // One `near` class cannot serve two loaders, so when either mirror's
    // class (an array's element) is a user-defined loader's, the `type` is
    // rebuilt from the mirrors themselves (`make_method_type`, the
    // `methodType(Class, Class)` bridge's builder) after the mint. Under
    // `CRATONVM_CLASS_INNER_CLASSES_CHECK`, as `identity` / `constant`.
    let ret_mirror = match args.first() {
        Some(Value::Object(Some(m))) => *m,
        _ => ex_mirror,
    };
    let user_defined = |ctx: &dyn NativeContext, m: ObjectRef| {
        mirror_class_id(ctx, m)
            .map(|id| mh_element_class_id(ctx, id))
            .is_some_and(|id| ctx.loader_id_of_class(id) >= 3)
    };
    let rebuild = (user_defined(&*ctx, ex_mirror)
        || (!crate::lang_class::mirror_is_primitive(&*ctx, ret_mirror)
            && user_defined(&*ctx, ret_mirror)))
        && crate::lang_class::class_inner_classes_check_enabled();
    if !rebuild {
        let handle = alloc_method_handle(ctx, "", "throwException", &desc, MH_KIND_THROW)?;
        ctx.set_field(handle, MH_BOUND, Value::Object(None));
        return Ok(Some(Value::Object(Some(handle))));
    }
    // GC-safety: the mint, the `Class[]` and the `MethodType` each allocate.
    let ret_pin = ctx.pin_native_root(ret_mirror);
    let ex_pin = ctx.pin_native_root(ex_mirror);
    let minted = alloc_method_handle(ctx, "", "throwException", &desc, MH_KIND_THROW);
    let handle = match minted {
        Ok(h) => h,
        Err(e) => {
            ctx.unpin_native_roots(ret_pin);
            return Err(e);
        }
    };
    let handle_pin = ctx.pin_native_root(handle);
    let arr = new_class_array(ctx, 1);
    let ex_mirror = ctx.read_native_pin(ex_pin, ex_mirror);
    ctx.set_array_element(arr, 0, Value::Object(Some(ex_mirror)));
    let ret_mirror = ctx.read_native_pin(ret_pin, ret_mirror);
    let mt = make_method_type(ctx, ret_mirror, Some(arr));
    let handle = ctx.read_native_pin(handle_pin, handle);
    ctx.unpin_native_roots(ret_pin);
    if let Ok(Some(Value::Object(Some(mt)))) = mt {
        ctx.set_field_by_name(handle, "type", Value::Object(Some(mt)));
    }
    ctx.set_field(handle, MH_BOUND, Value::Object(None));
    Ok(Some(Value::Object(Some(handle))))
}

/// `MethodHandles.identity(Class type)` — a *functional* shim returning an
/// `MH_KIND_IDENTITY` handle of type `(type)type` that returns its argument.
///
/// Promoted to the real-JDK essentials path + `check_override`-allow-listed
/// for the same reason as `constant`: in real-JDK mode the genuine
/// `MethodHandles.identity` bytecode yields a real
/// `MethodHandleImpl$IntrinsicMethodHandle` (and, via its primitive paths, a
/// `BoundMethodHandle` species), a real handle the `MH_KIND_*` shims cannot
/// read — so `identity().invoke()` / `.bindTo()` fail (OOB field reads on
/// slots 16–19). Groovy's `IndyInterface` and any `SwitchPoint`/dispatch chain
/// that threads values through `identity` needs this.
pub(crate) fn register_method_handles_identity_bridge(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "java/lang/invoke/MethodHandles",
        "identity",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let ty =
                match args.first() {
                    Some(Value::Object(Some(m))) => mirror_to_descriptor(ctx, *m).into_owned(),
                    // `identity(null)` (wave 40).
                    Some(Value::Object(None)) => return Err(mh_factory_null_argument(Some(
                        "Cannot invoke \"java.lang.Class.isPrimitive()\" because \"type\" is null",
                    ))),
                    _ => DESC_OBJECT.to_string(),
                };
            // `identity(void.class)`: `MethodType.methodType(void, void)`'s
            // refusal (wave 23; it minted a `(void)void` handle).
            if ty == "V" {
                return Err(mh_illegal_argument(
                    "parameter type cannot be void".to_string(),
                ));
            }
            let desc = format!("({ty}){ty}");
            // JIT round 17 wave 8, lane mh7: the handle's `type` names the
            // argument's OWN class, not whichever class of that name a
            // loader-blind lookup finds first (`MethodHandles.identity(c)` for a
            // `c` a user-defined loader defines; `R17Mh7Inner identity-*`).
            // Only a reference type of a user-defined loader, under
            // `CRATONVM_CLASS_INNER_CLASSES_CHECK`.
            let near = match args.first() {
                Some(Value::Object(Some(m))) if ty.starts_with('L') || ty.starts_with('[') => {
                    mirror_class_id(&*ctx, *m)
                        .map(|id| mh_element_class_id(&*ctx, id))
                        .filter(|&id| ctx.loader_id_of_class(id) >= 3)
                        .filter(|_| crate::lang_class::class_inner_classes_check_enabled())
                }
                _ => None,
            };
            let handle = alloc_method_handle_with_receiver(
                ctx,
                "",
                "",
                &desc,
                MH_KIND_IDENTITY,
                None,
                near,
            )?;
            Ok(Some(Value::Object(Some(handle))))
        },
    );
    r.set_category(__prev_cat);
}

/// `CallSite.dynamicInvoker()` (concrete on `MutableCallSite` /
/// `VolatileCallSite`) — a *functional* shim returning an
/// `MH_KIND_DYNAMIC_INVOKER` handle bound to the call site. On invocation it
/// reads the site's current `target` and delegates to it.
///
/// Promoted to the real-JDK essentials path + `check_override`-allow-listed
/// because the real `CallSite.makeDynamicInvoker` does
/// `getTargetHandle().bindArgumentL(0, this)` — a `BoundMethodHandle`
/// construction that hangs on CratonVM (no real species machinery).
/// `SwitchPoint.<init>` calls `mcs.dynamicInvoker()`, so without this shim
/// every `new SwitchPoint()` — and therefore Apache Groovy's
/// `IndyInterface.<clinit>` at runtime — hangs.
pub(crate) fn register_callsite_dynamic_invoker_bridge(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    for cs in [
        "java/lang/invoke/MutableCallSite",
        "java/lang/invoke/VolatileCallSite",
    ] {
        r.register(
            cs,
            "dynamicInvoker",
            "()Ljava/lang/invoke/MethodHandle;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                // Derive the invoker's descriptor from the current target so
                // `mh.type()` / arity checks see the right shape; default to a
                // nullary Object-returning type when the target is unreadable.
                let desc =
                    callsite_target_desc(ctx, this).unwrap_or_else(|| format!("(){DESC_OBJECT}"));
                let handle = alloc_method_handle(ctx, "", "", &desc, MH_KIND_DYNAMIC_INVOKER)?;
                ctx.set_field(handle, MH_BOUND, Value::Object(Some(this)));
                Ok(Some(Value::Object(Some(handle))))
            },
        );
    }
    r.set_category(__prev_cat);
}

/// `MethodHandles.dropArgumentChecks`' position refusal for
/// [`make_drop_arguments_adapter`], or `None` (an accepted position, or a
/// target whose `type` this model cannot vouch for).
pub(crate) fn drop_arguments_refusal(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    pos: i32,
    value_types: ObjectRef,
    dropped: usize,
) -> Option<MethodCallFailed> {
    let old = cs_target_type(ctx, target)?;
    let (params, _) = split_descriptor_params(&old)?;
    let outargs = params.len();
    if pos >= 0 && pos as usize <= outargs {
        return None;
    }
    let shown = method_type_display_in(&*ctx, &old)?;
    let array = format!(
        "[Ljava.lang.Class;@{:x}",
        ctx.identity_hash_code(value_types) as u32
    );
    Some(combinator_iae(format!(
        "no argument type to remove[{shown}, {pos}, {array}, {}, {outargs}]",
        outargs + dropped
    )))
}

pub(crate) fn make_drop_arguments_adapter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let orig_mh = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pos_arg = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let pos = pos_arg as usize;
    let extra_classes = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            return Ok(Some(Value::Object(Some(orig_mh))));
        }
    };
    let extra_n = ctx.array_length(extra_classes);
    // `dropArgumentChecks` (JDK 25), before a zero-length drop returns the
    // target: a position outside `0..=parameterCount` is an
    // `IllegalArgumentException` whose text is `Arrays.asList(oldType, pos,
    // valueTypes, inargs, outargs)` (the array by its `Object.toString()`).
    // Judged only when the target's `type` is authoritative
    // (`cs_target_type`); `--jdk-only`, as the other combinator checks.
    // Interpreter round i1 wave 47, lane L4, probe
    // `L4W47CombinatorChecksOverConstants`; the adapter was built and read
    // its arguments at a clamped position.
    if mh_combinator_checks_in_this_mode(&*ctx) {
        if let Some(refusal) = drop_arguments_refusal(ctx, orig_mh, pos_arg, extra_classes, extra_n)
        {
            return Err(refusal);
        }
    }
    if extra_n == 0 {
        return Ok(Some(Value::Object(Some(orig_mh))));
    }
    // Construct the widened descriptor from the original MH's effective
    // type(), not its raw leaf descriptor. Groovy stacks dropArguments on top
    // of asType/insert/collect adapters whose MH_DESC can still describe a
    // lower-level leaf; widening that raw shape overstates arity and makes the
    // guard test receive the wrong arguments forever.
    let inner_desc = mh_type_descriptor(ctx, orig_mh)
        .or_else(|| mh_read_desc(ctx, orig_mh))
        .unwrap_or_default();
    let widened_desc = widen_descriptor(ctx, &inner_desc, extra_classes, pos);
    // Encode `pos:extra_n` into MH_CLASS so dispatch can recover BOTH
    // the drop position and the exact number of dropped values directly,
    // instead of re-deriving drop count later from an arg-count
    // difference (see the dispatch-side comment on why that heuristic
    // was wrong for chained/nested combinators).
    let pos_str = format!("{pos}:{extra_n}");
    // GC-safety: `alloc_method_handle` allocates a new MethodHandle object,
    // which can trigger a collection that relocates `orig_mh`/`extra_classes`
    // (both captured well before this point and read again below). Pin them
    // and re-read the forwarded references before their next use.
    let orig_mh_pin = ctx.pin_native_root(orig_mh);
    let extra_classes_pin = ctx.pin_native_root(extra_classes);
    let wrapper = alloc_method_handle(ctx, &pos_str, "drop", &widened_desc, MH_KIND_DROP)?;
    // gen r4w3/rooting: `wrapper` is written and returned after the
    // MethodType allocation below (a GC point); pin it.
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let orig_mh = ctx.read_native_pin(orig_mh_pin, orig_mh);
    let extra_classes = ctx.read_native_pin(extra_classes_pin, extra_classes);
    ctx.set_field(wrapper, MH_BOUND, Value::Object(Some(orig_mh)));
    // Also widen the `type:MethodType` field so JDK-internal code
    // that reads mh.type().parameterCount() sees the widened arity.
    let orig_type = ctx.get_field(orig_mh, 0);
    if let Value::Object(Some(mt)) = orig_type {
        let ret = ctx.get_field(mt, 0);
        // gen r4w3/rooting: `ret` is stored into the new MethodType after
        // its allocation (a GC point); pin it.
        let ret_pin = pin_mh_value(ctx, ret);
        if let Value::Object(Some(orig_ptypes)) = ctx.get_field(mt, 1) {
            let orig_n = ctx.array_length(orig_ptypes);
            let new_n = orig_n + extra_n;
            // GC-safety: `new_array` can trigger a collection that relocates
            // `orig_ptypes` (and re-covers `extra_classes`, still pinned
            // above); pin/re-read before the element-copy loops below.
            let orig_ptypes_pin = ctx.pin_native_root(orig_ptypes);
            let new_ptypes = new_class_array(ctx, new_n);
            let new_ptypes_pin = ctx.pin_native_root(new_ptypes);
            let orig_ptypes = ctx.read_native_pin(orig_ptypes_pin, orig_ptypes);
            let extra_classes = ctx.read_native_pin(extra_classes_pin, extra_classes);
            let pos_c = pos.min(orig_n);
            for i in 0..pos_c {
                let v = ctx.get_array_element(orig_ptypes, i);
                ctx.set_array_element(new_ptypes, i, v);
            }
            for i in 0..extra_n {
                let v = ctx.get_array_element(extra_classes, i);
                ctx.set_array_element(new_ptypes, pos_c + i, v);
            }
            for i in pos_c..orig_n {
                let v = ctx.get_array_element(orig_ptypes, i);
                ctx.set_array_element(new_ptypes, extra_n + i, v);
            }
            // GC-safety: `alloc_concurrent_synthetic` below can trigger a
            // collection that relocates `new_ptypes` (fully populated above,
            // read again once the new MethodType wraps it).
            let new_mt: ObjectRef =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6)?;
            let new_ptypes = ctx.read_native_pin(new_ptypes_pin, new_ptypes);
            let ret = read_pinned_mh_arg(ctx, ret_pin, ret);
            ctx.unpin_native_roots(orig_ptypes_pin);
            ctx.set_field(new_mt, 0, ret);
            ctx.set_field(new_mt, 1, Value::Object(Some(new_ptypes)));
            // gen r4w3/rooting: refreshed in place across the form allocation.
            let mut new_mt = new_mt;
            populate_method_type_form(ctx, &mut new_mt)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            ctx.set_field_by_name(wrapper, "type", Value::Object(Some(new_mt)));
        }
    }
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    ctx.unpin_native_roots(orig_mh_pin);
    Ok(Some(Value::Object(Some(wrapper))))
}

/// Functional `MethodHandles.insertArguments` / `MethodHandle.asCollector`
/// plus the `CallSite`-construction natives Apache Groovy's `IndyInterface`
/// fallback relies on (`CallSite.makeUninitializedCallSite`,
/// `MutableCallSite.setTarget`). Promoted to the real-JDK essentials path +
/// `check_override`-allow-listed.
///
/// Why these are needed in real-JDK mode:
/// * `insertArguments` was a no-op synthetic stub (it dropped the bound
///   values); in real-JDK boot the genuine bytecode builds a
///   `BoundMethodHandle` species the VM can't run.  Groovy binds the call
///   site + dispatch metadata into its fallback handle via `insertArguments`.
/// * `asCollector` is unimplemented (real bytecode → species); Groovy uses
///   `asCollector(Object[].class, n)` to gather the call site's spread args.
/// * `new MutableCallSite(MethodType)` (Groovy's `CacheableCallSite`) runs
///   `CallSite.makeUninitializedCallSite`, which NPEs on CratonVM because
///   `MethodTypeForm.methodHandles` (a lazy cache array) is null. Shim it to
///   a typed inert placeholder (the caller `setTarget`s a real target before
///   the site is ever invoked).
/// * `MutableCallSite.setTarget`'s real `checkTargetChange` compares the new
///   target's `MethodType` to the site's; synthetic `MethodType`s don't
///   `equals()` the JDK forms → `WrongMethodTypeException`. Shim it to store
///   the target field directly (dispatch ignores types anyway).
pub(crate) fn register_method_handle_combinator_extras_bridge(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);

    // MethodHandles.insertArguments(target, pos, values[]) → MH_KIND_INSERT
    r.register(
        "java/lang/invoke/MethodHandles",
        "insertArguments",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        native_insert_arguments,
    );

    // MethodHandle.asCollector(arrayType, count) → MH_KIND_COLLECT
    r.register(
        "java/lang/invoke/MethodHandle",
        "asCollector",
        "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if let Some(answer) =
                mh_foreign_adapter_through_jdk(ctx, args, "asCollector", MH_AS_COLLECTOR_DESC)
            {
                return answer;
            }
            let target = obj_arg(args, 0)?;
            // Interpreter round i5 wave 100, lane L4 (probe
            // `L4W100NullArrayTypeAdapters`): `asCollectorChecks` ->
            // `spreadArrayChecks` dereferences a null `arrayType`
            // (`getComponentType()`), as the three-argument form already
            // answers; this built a collector into `Object[]`.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(mh_null_array_type());
            }
            let count = match args.get(2) {
                Some(Value::Int(c)) => *c,
                _ => 0,
            };
            // `asCollectorChecks` (wave 23): a non-array type, an illegal
            // length, a handle with no parameter to collect into, or an
            // array type the trailing parameter cannot hold.
            if let (Some(array_desc), Some(tdesc)) = (
                mh_mirror_descriptor_strict(&*ctx, args.get(1)),
                mh_type_descriptor(ctx, target),
            ) {
                if let Some((params, _)) = split_descriptor_params(&tdesc) {
                    let pos = params.len() as i64 - 1;
                    if let Some(refusal) =
                        mh_collector_refusal(&*ctx, &tdesc, &array_desc, pos, count)
                    {
                        return Err(refusal);
                    }
                }
            }
            // Slot 2 is the `arrayType` Class mirror, and it is the whole of
            // W7-19's `asCollector` fix. Without it the `MH_KIND_COLLECT` arm
            // has no way to learn the collector's ARRAY TYPE — the carrier
            // held only (target, count) and the arm therefore gathered into an
            // `Object[]` for every collector, so `sumAll(int[])
            // .asCollector(int[].class, 3).invoke(1,2,3)` handed `sumAll` a
            // reference array and answered 0 where HotSpot 25 answers 6.
            // A mirror is a REFERENCE, so it goes in a carrier slot without
            // the int-in-an-oop-slot hazard §5 is about; the alternative
            // (an encoded element-type tag) would be exactly that hazard.
            let arr_cls = match args.get(1) {
                Some(Value::Object(Some(c))) => Some(*c),
                _ => None,
            };
            // gc-common w20-f: descriptor read and `target` / `arrayType`
            // pinned BEFORE the carrier's allocation (they were stored and
            // pinned after it, at pre-allocation addresses); `target_pin`
            // (lowest) releases all at the end.
            let desc = mh_carrier_leaf_desc(ctx, target);
            let target_pin = ctx.pin_native_root(target);
            let arr_cls_pin = arr_cls.map(|c| ctx.pin_native_root(c));
            let wrapper = alloc_mh_carrier_holding(
                ctx,
                "__mh_collect_wrapper__",
                &[
                    Value::Object(Some(target)),
                    Value::Int(count),
                    Value::Object(arr_cls),
                ],
            );
            // gen r4w3/rooting: `alloc_method_handle` and
            // `build_method_type_from_descriptor` are GC points; `wrapper`,
            // `target`, the `arrayType` mirror (args[1] — a slice re-read is
            // stale too) and `adapter` are all used after them. Pin and
            // re-read.
            let wrapper_pin = ctx.pin_native_root(wrapper);
            let adapter =
                alloc_method_handle(ctx, "__adapter__", "collect", &desc, MH_KIND_COLLECT)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            let target = ctx.read_native_pin(target_pin, target);
            let arr_cls = match (arr_cls, arr_cls_pin) {
                (Some(c), Some(p)) => Some(ctx.read_native_pin(p, c)),
                _ => None,
            };
            let adapter_pin = ctx.pin_native_root(adapter);
            ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
            // type(): asCollector REPLACES the trailing array parameter with
            // `count` parameters of the array's component type (HotSpot:
            // `(Object[])R`.asCollector(Object[],2) -> `(Object,Object)R`).
            // Chain off the target's adapted type.
            if let Some(tdesc) = mh_type_descriptor(ctx, target) {
                if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                    if !params.is_empty() {
                        params.pop(); // drop the trailing array parameter
                    }
                    // Component descriptor of the array type arg (args[1]).
                    // gen r4w3/rooting: the pinned, re-read copy of args[1].
                    let comp = match arr_cls {
                        Some(arr_cls) => {
                            let ad = mirror_to_descriptor(ctx, arr_cls);
                            if let Some(rest) = ad.strip_prefix('[') {
                                rest.to_string()
                            } else {
                                DESC_OBJECT.to_string()
                            }
                        }
                        _ => DESC_OBJECT.to_string(),
                    };
                    for _ in 0..count.max(0) {
                        params.push(comp.clone());
                    }
                    let new_desc = format!("({}){}", params.concat(), ret);
                    if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &new_desc) {
                        let adapter = ctx.read_native_pin(adapter_pin, adapter);
                        ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
                    }
                }
            }
            let adapter = ctx.read_native_pin(adapter_pin, adapter);
            ctx.unpin_native_roots(target_pin);
            Ok(Some(Value::Object(Some(adapter))))
        },
    );

    // MethodHandle.asSpreader(arrayType, count) → MH_KIND_SPREAD
    r.register(
        "java/lang/invoke/MethodHandle",
        "asSpreader",
        "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if let Some(answer) =
                mh_foreign_adapter_through_jdk(ctx, args, "asSpreader", MH_AS_SPREADER_DESC)
            {
                return answer;
            }
            let target = obj_arg(args, 0)?;
            // Interpreter round i5 wave 100, lane L4 (probe
            // `L4W100NullArrayTypeAdapters`): `asSpreaderChecks` ->
            // `spreadArrayChecks`' null `arrayType` is the NPE; this built a
            // spreader over `Object[]`.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(mh_null_array_type());
            }
            let count = match args.get(2) {
                Some(Value::Int(c)) => *c,
                _ => 0,
            };
            // gen r4w3/rooting: `alloc_method_handle` and
            // `build_method_type_from_descriptor` are GC points; `wrapper`,
            // `target`, the `arrayType` mirror (args[1] — a slice re-read is
            // stale too) and `adapter` are all used after them. Pin and
            // re-read.
            //
            // gc-common w20-f: `target` and the mirror are pinned BEFORE the
            // carrier's allocation (they were stored and pinned after it, at
            // pre-allocation addresses); `target_pin` (lowest) releases all at
            // the end.
            // `spreadArrayChecks` + `asSpreaderChecks`' length test (wave
            // 23): "not an array type: ...", "bad spread array length".
            if let (Some(array_desc), Some(tdesc)) = (
                mh_mirror_descriptor_strict(&*ctx, args.get(1)),
                mh_type_descriptor(ctx, target),
            ) {
                if let Some(refusal) = mh_spread_array_refusal(&*ctx, &array_desc, count) {
                    return Err(refusal);
                }
                if let Some((params, ret)) = split_descriptor_params(&tdesc) {
                    if (params.len() as i64) < i64::from(count) || count < 0 {
                        return Err(mh_illegal_argument("bad spread array length".to_string()));
                    }
                    // Interpreter round i4 wave 98, lane L4 (probe
                    // `L4W98SpreaderConversion`): JDK 25 `asSpreaderChecks`
                    // asks `MethodType.canConvert(arrayElement, ptype)` for
                    // each spread parameter and, on a refusal, elicits
                    // `asType(needType)`'s `WrongMethodTypeException`
                    // (`cannot convert MethodHandle(String,String)String to
                    // (int,int)String`). This built the spreader, and the
                    // conversion failed (or did not) only at the call.
                    if let Some(element) = array_desc.strip_prefix('[') {
                        let start = params.len() - count as usize;
                        let fail = params[start..]
                            .iter()
                            .any(|p| p != element && !mh_can_convert(element, p));
                        if fail {
                            let mut need = params.clone();
                            for p in &mut need[start..] {
                                *p = element.to_string();
                            }
                            let need_desc = format!("({}){}", need.concat(), ret);
                            if let (Some(old_shown), Some(new_shown)) = (
                                method_type_display_in(&*ctx, &tdesc),
                                method_type_display_in(&*ctx, &need_desc),
                            ) {
                                return Err(crate::phases_early::throw_jca_exc(
                                    ctx,
                                    "java/lang/invoke/WrongMethodTypeException",
                                    &format!("cannot convert MethodHandle{old_shown} to {new_shown}"),
                                ));
                            }
                        }
                    }
                }
            }
            let arr_cls = match args.get(1) {
                Some(Value::Object(Some(c))) => Some(*c),
                _ => None,
            };
            let desc = mh_carrier_leaf_desc(ctx, target);
            let target_pin = ctx.pin_native_root(target);
            let arr_cls_pin = arr_cls.map(|c| ctx.pin_native_root(c));
            let wrapper = alloc_mh_carrier_holding(
                ctx,
                "__mh_spread_wrapper__",
                &[Value::Object(Some(target)), Value::Int(count)],
            );
            let wrapper_pin = ctx.pin_native_root(wrapper);
            let adapter = alloc_method_handle(ctx, "__adapter__", "spread", &desc, MH_KIND_SPREAD)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            let target = ctx.read_native_pin(target_pin, target);
            let arr_cls = match (arr_cls, arr_cls_pin) {
                (Some(c), Some(p)) => Some(ctx.read_native_pin(p, c)),
                _ => None,
            };
            let adapter_pin = ctx.pin_native_root(adapter);
            ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
            // type(): asSpreader REPLACES the trailing `count` parameters with
            // a SINGLE parameter of the array type — the exact inverse of
            // `asCollector` above (HotSpot: `(A,B)R`.asSpreader(Object[],2) ->
            // `(Object[])R`). Chain off the target's ADAPTED type, not its raw
            // `MH_DESC`, so a stack of adapters tracks arity.
            //
            // Leaving `type` at the target's own signature is not a cosmetic
            // gap. `alloc_method_handle` populates `type` from `desc`, so the
            // spreader claimed the UNSPREAD shape, and the very next thing
            // every real caller does is `asType` to the spread shape:
            // Groovy's `IndyInterface.fallback` does
            // `handle.asSpreader(Object[].class, arguments.length)
            //  .asType(methodType(Object.class, Object[].class))`
            // on every `invokedynamic` dispatch. `mh_astype_refusal` then
            // refused a 2-param → 1-param conversion HotSpot never sees,
            // throwing `WrongMethodTypeException: cannot convert
            // MethodHandle(beans,Closure)Object to (Object[])Object` — the
            // refusal was right, the type it was reading was wrong.
            if let Some(tdesc) = mh_type_descriptor(ctx, target) {
                if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                    let n = count.max(0) as usize;
                    if params.len() >= n {
                        params.truncate(params.len() - n);
                        // The array type arg (args[1]) spelled as a descriptor.
                        // gen r4w3/rooting: the pinned, re-read copy of args[1].
                        let arr_desc = match arr_cls {
                            Some(arr_cls) => mirror_to_descriptor(ctx, arr_cls).into_owned(),
                            _ => "[Ljava/lang/Object;".to_string(),
                        };
                        params.push(arr_desc);
                        let new_desc = format!("({}){}", params.concat(), ret);
                        if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &new_desc) {
                            let adapter = ctx.read_native_pin(adapter_pin, adapter);
                            ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
                        }
                    }
                }
            }
            let adapter = ctx.read_native_pin(adapter_pin, adapter);
            ctx.unpin_native_roots(target_pin);
            Ok(Some(Value::Object(Some(adapter))))
        },
    );

    // MethodHandle.asVarargsCollector(arrayType) / MethodHandle.asFixedArity()
    // → the receiver, with only the `MH_VARARGS` marking changed.
    //
    // Why the real bytecode cannot run here: `asVarargsCollector` wraps the
    // receiver in `MethodHandleImpl$AsVarargsCollector`, a
    // `DelegatingMethodHandle` whose constructor runs
    // `chooseDelegatingForm` → `DelegatingMethodHandle.makeReinvokerForm` →
    // `mtype.form().cachedLambdaForm(LF_DELEGATE)`, i.e. it demands a real
    // `MethodTypeForm` cache and then a real `LambdaForm` reinvoker that would
    // have to re-enter the receiver through `invokeBasic`. CratonVM's handles
    // are the `MH_KIND_*` shim model, which has no `invokeBasic` body, so even
    // a fully-populated form only moves the failure one frame deeper.
    // (regression-suite `RJdkHandles.adaptation` line 137 died on the FIRST of
    // those two: `Cannot load from object array because "this.lambdaForms" is
    // null` — in BOTH --real-jdk and --jdk-only, while HotSpot 25 passes.
    // `populate_method_type_form` now allocates that cache, but this handle
    // must still never reach the reinvoker.)
    //
    // Why the identity is the right shim rather than a new adapter kind:
    // CratonVM applies varargs-collector semantics at DISPATCH, not on the
    // handle — `collect_trailing_varargs` collects the excess arguments of an
    // `invoke` that supplies more flat values than the target descriptor
    // declares into a fresh array of the array-typed parameter's component
    // type. That trigger is arity/shape driven, so a "this handle is a
    // collector" marking adds nothing to the collection trigger. (The
    // `MH_VARARGS` marking below is no longer reflection-only: dispatch does
    // read it -- `MH_MARKS_DIVERT_DIRECT_LANE` keeps a marked handle off the
    // direct lane, and the field links refuse one; j27 wave 3 lane BACKLOG,
    // MH3-8 retired the old "never consulted by dispatch" note.)
    // `asFixedArity()` is the inverse
    // and is likewise the identity: `collect_trailing_varargs` returns an
    // already-packed call (exactly N args, an array in the array slot)
    // untouched, which is precisely fixed-arity behaviour.
    //
    // W7-19 §5.2, the former "known deviation": `isVarargsCollector()` used to
    // answer `false` for every handle, including one just returned by
    // `asVarargsCollector`, because the marking was stored nowhere. It is now
    // stored — in `MH_VARARGS`, the sixth synthetic slot, set here and cleared
    // by `asFixedArity` — and read back by an `isVarargsCollector` native
    // registered below. Both writers are width-guarded (see `MH_VARARGS`), so a
    // handle minted by some other allocator is untouched and still answers
    // `false`, exactly as before.
    //
    // The DECLARED DEVIATION this block used to record (one handle, so the
    // marking leaked through the receiver) is gone: since 2026-08-21 both
    // natives mint a COPY through `mh_with_varargs_marking` /
    // `mh_clone_handle` and leave the receiver as it was. What remained was
    // identity (wave 24, `tools/probes/interp/L4/L4W24CallSiteAndArity.java`):
    // HotSpot hands back the RECEIVER itself when it already has the
    // requested arity kind -- `asFixedArity()` of a fixed-arity handle
    // (`MethodHandle.asFixedArity`: `return this`) and `asVarargsCollector(t)`
    // of a collector whose trailing parameter IS `t`
    // (`isVarargsCollector() && lastMatch`) -- and a copy was minted for both.
    r.register(
        "java/lang/invoke/MethodHandle",
        "asVarargsCollector",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if let Some(answer) = mh_foreign_adapter_through_jdk(
                ctx,
                args,
                "asVarargsCollector",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ) {
                return answer;
            }
            // Interpreter round i5 wave 100, lane L4 (probe
            // `L4W100NullArrayTypeAdapters`): JDK 25 starts with
            // `Objects.requireNonNull(arrayType)` (a message-less NPE); a null
            // `arrayType` was marked a varargs collector here.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(mh_factory_null_argument(None));
            }
            // `asCollectorChecks(arrayType, parameterCount - 1, 0)` (wave
            // 23): `add(int,int).asVarargsCollector(int.class)` is "not an
            // array type: int" on HotSpot, and was accepted here.
            let mut last_match = false;
            // Round 16 (lane mh, MH5-2): the type `makeVarargsCollector`
            // retypes a target to when its trailing parameter is not
            // `arrayType` (see `mh_as_varargs_collector_retyped`).
            let mut retype: Option<String> = None;
            if let Some(Value::Object(Some(target))) = args.first() {
                let target = *target;
                if let (Some(array_desc), Some(tdesc)) = (
                    mh_mirror_descriptor_strict(&*ctx, args.get(1)),
                    mh_type_descriptor(ctx, target),
                ) {
                    if let Some((params, ret)) = split_descriptor_params(&tdesc) {
                        let pos = params.len() as i64 - 1;
                        if let Some(refusal) =
                            mh_collector_refusal(&*ctx, &tdesc, &array_desc, pos, 0)
                        {
                            return Err(refusal);
                        }
                        last_match = params.last().is_some_and(|p| *p == array_desc);
                        if !last_match && !params.is_empty() {
                            retype = Some(format!(
                                "({}{}){}",
                                params[..params.len() - 1].concat(),
                                array_desc,
                                ret
                            ));
                        }
                    }
                }
            }
            let keep_receiver = last_match
                && matches!(args.first(), Some(Value::Object(Some(t))) if mh_is_varargs_collector(&*ctx, *t));
            if keep_receiver {
                return Ok(args.first().copied());
            }
            if let Some(new_desc) = retype.filter(|_| mh_varargs_array_type_enabled()) {
                if let Some(answer) =
                    mh_as_varargs_collector_retyped(ctx, args.first().copied(), &new_desc)
                {
                    return answer;
                }
            }
            mh_with_varargs_marking(ctx, args.first().copied(), true)
        },
    );
    r.register(
        "java/lang/invoke/MethodHandle",
        "asFixedArity",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // A fixed-arity handle is its own fixed-arity form (see above).
            if let Some(Value::Object(Some(this))) = args.first() {
                if !mh_is_varargs_collector(&*ctx, *this) {
                    return Ok(args.first().copied());
                }
            }
            mh_with_varargs_marking(ctx, args.first().copied(), false)
        },
    );
    // `MethodHandle.isVarargsCollector()` is CONCRETE in the real JDK — the base
    // class returns `false` and `MethodHandleImpl$AsVarargsCollector` overrides
    // it — so this registration shadows real bytecode. That is the default per
    // `docs/architecture/natives-over-real-jdk-classes.md` §1: on the cold
    // interpreter paths a registered native beats bytecode with no list
    // consulted, and this block's ambient `NativeKind` is `Bridge`, which
    // `--jdk-only` keeps. The warm/cached/JIT paths reinstate the preference
    // from `vm_exec.rs`'s `check_override` mirror, which lists
    // `asVarargsCollector`/`asFixedArity` but not this method; a lane that owns
    // that file should add it, and until then the only cost is that a JIT-warm
    // caller may read the base class's `false` instead of the marking. Answering
    // `false` is what the whole VM did before this change, so the fallback is
    // the old behaviour rather than a new wrong answer.
    r.register(
        "java/lang/invoke/MethodHandle",
        "isVarargsCollector",
        "()Z",
        |ctx, args| {
            let flagged = match args.first() {
                Some(Value::Object(Some(this))) => mh_is_varargs_collector(ctx, *this),
                _ => false,
            };
            Ok(Some(Value::Int(if flagged { 1 } else { 0 })))
        },
    );

    // Interpreter round i1 wave 45, lane L4 (host run of
    // `L4W44CombinatorHunt`): four JDK bodies over CratonVM's handles end in
    // `rebind().editor()` (`BoundMethodHandle`'s LambdaForm editor, which a
    // synthetic handle has not: `NoSuchMethodError: ... MethodHandle.editor()`)
    // or answer from `isVarargsCollector()` alone. Each is expressed through
    // the natives above instead; see each function.
    r.register(
        "java/lang/invoke/MethodHandle",
        "withVarargs",
        "(Z)Ljava/lang/invoke/MethodHandle;",
        mh_with_varargs,
    );
    r.register(
        "java/lang/invoke/MethodHandle",
        "asCollector",
        "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;",
        mh_as_collector_at,
    );
    r.register(
        "java/lang/invoke/MethodHandle",
        "asSpreader",
        "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;",
        mh_as_spreader_at,
    );
    r.register(
        "java/lang/invoke/MethodHandles",
        "dropReturn",
        "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        mhs_drop_return,
    );

    // MethodHandles.explicitCastArguments(target, newType) → passthrough that
    // stamps the new type (mirrors the `asType` shim). The real bytecode runs
    // strict `explicitCastArgumentsChecks` that rejects synthetic handles whose
    // MethodType doesn't match the JDK form (WrongMethodTypeException). STATIC:
    // args[0]=target MH, args[1]=newType.
    //
    // G31 — **ARITY ONLY, and that is the whole finding.** It is tempting to
    // reuse `asType`'s convertibility predicate here because the two methods
    // sit beside each other in `MethodHandles` and read alike. MEASURED on
    // HotSpot 25.0.3+9 over the same 613-cell sweep that produced `asType`'s
    // matrices: `explicitCastArguments` accepts **every** type pair in both the
    // return and the parameter position — `String -> boolean`, `double -> char`,
    // `int[] -> long`, all of them — because it inserts an explicit cast (a
    // primitive narrowing, an unbox-or-zero, a checked reference cast) instead
    // of demanding the conversion be lossless. Its only refusal is a parameter
    // COUNT mismatch. Applying `asType`'s rule here would refuse 248 pairs
    // HotSpot performs.
    //
    // The message is its own, transcribed: `cannot explicitly cast
    // MethodHandle(int)void to ()void` — "explicitly cast", not "convert".
    r.register(
        "java/lang/invoke/MethodHandles",
        "explicitCastArguments",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if let (Some(Value::Object(Some(t))), Some(Value::Object(Some(mt)))) =
                (args.first(), args.get(1))
            {
                let (t, mt) = (*t, *mt);
                if let Some(refusal) = mh_explicit_cast_refusal(ctx, t, mt) {
                    return Err(refusal);
                }
                // The leaf arms convert with `asType`'s rules and refuse what
                // they cannot convert (wave 26); an explicit cast narrows and
                // zeroes a `null` instead, so the copy is marked to keep the
                // lenient conversion (`MH_MARK_EXPLICIT_CAST`) -- by
                // `mh_with_stamped_type` since round 12 wave 7, which records a
                // stamp over a stamp as an explicit MIDDLE link instead. The
                // receiver itself (an unchanged type, answered as is) is not
                // marked.
                //
                // Round 15 wave 6 (lane mh5, MH4-1): and the copy of a
                // variable-arity target is FIXED-arity, as both of the JDK's
                // answers are (`target.asFixedArity().asType(newType)`, or
                // `makePairwiseConvert`'s adapter); the count is the
                // target's, so nothing was collected. Judged on the
                // receiver's address AFTER the copy (pinned): the receiver
                // itself is answered only for an unchanged type.
                if mh_is_varargs_collector(&*ctx, t) && mh_astype_fixed_collector_enabled() {
                    let pin = ctx.pin_native_root(t);
                    let answer = mh_with_stamped_type(ctx, Value::Object(Some(t)), mt, true);
                    let t = ctx.read_native_pin(pin, t);
                    ctx.unpin_native_roots(pin);
                    if let Ok(Some(Value::Object(Some(copy)))) = &answer {
                        if *copy != t {
                            mh_set_varargs_collector(&*ctx, *copy, false);
                        }
                    }
                    return answer;
                }
                return mh_with_stamped_type(ctx, Value::Object(Some(t)), mt, true);
            }
            // Interpreter round i7 wave 141, lane L4
            // (`CRATONVM_MH_COMBINATOR_NULL_TEXTS`, `L4W141CombinatorNullTexts`
            // rows `explicit-null-*`): JDK 25's `explicitCastArguments` reads
            // `target.type()` and then `newType.parameterCount()`
            // (`explicitCastArgumentsChecks`); a null either way answered the
            // first argument (a null handle, or the target unconverted).
            if mh_combinator_null_texts_enabled() {
                if !matches!(args.first(), Some(Value::Object(Some(_)))) {
                    return Err(combinator_npe_handle("target"));
                }
                if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
                    return Err(combinator_npe_invoking(
                        "java.lang.invoke.MethodType.parameterCount()",
                        "newType",
                    ));
                }
            }
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );

    // CallSite.makeUninitializedCallSite(MethodType) → typed inert placeholder.
    // Instance method: args[0] = this(CallSite), args[1] = MethodType.
    r.register(
        "java/lang/invoke/CallSite",
        "makeUninitializedCallSite",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let mt = match args.get(1) {
                Some(Value::Object(Some(mt))) => Some(*mt),
                _ => None,
            };
            // Invoking it throws HotSpot's `IllegalStateException` (the
            // `MH_KIND_CONSTANT` dispatch arm, wave 47).
            mh_alloc_typed(
                ctx,
                UNINITIALIZED_CALL_SITE_NAME,
                MH_KIND_CONSTANT,
                mt,
                None,
            )
        },
    );

    // MutableCallSite/VolatileCallSite.setTarget(MethodHandle) → store the
    // `target` field directly. The real `checkTargetChange` compares
    // `MethodType`s by reference, which synthetic MethodTypes defeat, so the
    // comparison is `cs_set_target_refusal`'s, by descriptor (wave 24; it was
    // skipped entirely and any target was stored).
    //
    // This registration runs AFTER `register_p60_callsite` (phase 65 vs phase
    // 60) and the registry is LAST-WINS, so it is the `setTarget` the VM
    // actually dispatches. It used to write `set_field_by_name(this, "target")`
    // while `getTarget` read slot 0: on the synthetic CallSite model the fields
    // are UNNAMED, so the by-name write silently no-op'd and `getTarget()` kept
    // handing back the constructor's target forever (`callsite_mutable_p60`).
    // Both halves now go through `cs_target_slot`.
    for cs in [
        "java/lang/invoke/MutableCallSite",
        "java/lang/invoke/VolatileCallSite",
    ] {
        r.register(
            cs,
            "setTarget",
            "(Ljava/lang/invoke/MethodHandle;)V",
            cs_set_target,
        );
    }

    r.set_category(__prev_cat);
}

pub(crate) const MH_AS_TYPE_DESC: &str = "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
pub(crate) const MH_AS_COLLECTOR_DESC: &str = "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;";
pub(crate) const MH_AS_SPREADER_DESC: &str = "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;";

/// The handle a `MethodHandle`-returning call answered, or an internal error
/// (every native called here answers a handle or throws).
pub(crate) fn mh_handle_answer(answer: Option<Value>, what: &str) -> Result<ObjectRef, MethodCallFailed> {
    match answer {
        Some(Value::Object(Some(mh))) => Ok(mh),
        _ => Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Internal {
                message: format!("{what} answered no MethodHandle"),
            },
        )),
    }
}

/// `mh.asType(<desc>)` through the registered `asType` native.
pub(crate) fn mh_as_type_to_desc(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    desc: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let pin = ctx.pin_native_root(mh);
    let mt = build_method_type_from_descriptor(ctx, desc);
    let mh = ctx.read_native_pin(pin, mh);
    ctx.unpin_native_roots(pin);
    let mt = match mt? {
        Some(mt) => mt,
        None => return Ok(mh),
    };
    let answer = ctx.invoke_virtual(mh, "asType", MH_AS_TYPE_DESC, &[Value::Object(Some(mt))])?;
    mh_handle_answer(answer, "asType")
}

/// `MethodHandles.permuteArguments(mh, <desc>, reorder)` through its native.
pub(crate) fn mh_permute_to_desc(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    desc: &str,
    reorder: &[i32],
) -> Result<ObjectRef, MethodCallFailed> {
    let mh_pin = ctx.pin_native_root(mh);
    let mt = match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        Ok(None) => {
            ctx.unpin_native_roots(mh_pin);
            return mh_handle_answer(None, "permuteArguments' MethodType");
        }
        Err(e) => {
            ctx.unpin_native_roots(mh_pin);
            return Err(e);
        }
    };
    let mt_pin = ctx.pin_native_root(mt);
    let order = ctx.new_array(ArrayElementType::Int, reorder.len());
    for (i, &r) in reorder.iter().enumerate() {
        ctx.set_array_element(order, i, Value::Int(r));
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.unpin_native_roots(mh_pin);
    let answer = mhs_permute_arguments(
        ctx,
        &[
            Value::Object(Some(mh)),
            Value::Object(Some(mt)),
            Value::Object(Some(order)),
        ],
    )?;
    mh_handle_answer(answer, "permuteArguments")
}

/// `spreadArrayChecks`' null `arrayType` (`arrayType.getComponentType()`).
pub(crate) fn mh_null_array_type() -> MethodCallFailed {
    mh_factory_null_argument(Some(
        "Cannot invoke \"java.lang.Class.getComponentType()\" because \"arrayType\" is null",
    ))
}

/// `MethodHandle.withVarargs(boolean)` (JDK 25; `AsVarargsCollector`
/// overrides it): `true` is `asVarargsCollector(type().lastParameterType())`,
/// or the handle itself when it already collects; `false` is the handle
/// itself, or `asFixedArity()` of a collector. The JDK body of the base class
/// answers `this` for `false`, which for a handle this model marked a
/// collector (`MH_VARARGS`, not a separate class) left it collecting
/// (`L4W44CombinatorHunt` row `withVarargs-false`: `true` where HotSpot
/// answers `false`).
pub(crate) fn mh_with_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(answer) = mh_foreign_adapter_through_jdk(
        ctx,
        args,
        "withVarargs",
        "(Z)Ljava/lang/invoke/MethodHandle;",
    ) {
        return answer;
    }
    let this = obj_arg(args, 0)?;
    let make = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    let collects = mh_is_varargs_collector(&*ctx, this);
    if make == collects {
        return Ok(Some(Value::Object(Some(this))));
    }
    if !make {
        return ctx.invoke_virtual(
            this,
            "asFixedArity",
            "()Ljava/lang/invoke/MethodHandle;",
            &[],
        );
    }
    let last = mh_type_descriptor(ctx, this)
        .and_then(|d| split_descriptor_params(&d))
        .and_then(|(params, _)| params.last().cloned())
        .unwrap_or_else(|| DESC_VOID.to_string());
    let pin = ctx.pin_native_root(this);
    let mirror = mirror_for_descriptor_name(ctx, &descriptor_to_class_name(&last));
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    ctx.invoke_virtual(
        this,
        "asVarargsCollector",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(mirror))],
    )
}

/// `MethodHandle.asCollector(int collectArgPos, Class arrayType, int
/// arrayLength)`. The JDK body builds a `LambdaForm` with `rebind().editor()`,
/// which a CratonVM handle has not. `asCollectorChecks` first (the shared
/// `mh_collector_refusal`); at the trailing position it is the two-argument
/// `asCollector` native; elsewhere it is `collectArguments(this, pos,
/// identity(arrayType).asCollector(arrayType, n))`, the collector adapted to
/// the parameter's own type when that is a supertype of `arrayType`, whose
/// type is `MethodType.asCollectorType`'s.
pub(crate) fn mh_as_collector_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(answer) = mh_foreign_adapter_through_jdk(
        ctx,
        args,
        "asCollector",
        "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;",
    ) {
        return answer;
    }
    let target = obj_arg(args, 0)?;
    let pos = match args.get(1) {
        Some(Value::Int(p)) => *p,
        _ => 0,
    };
    let array_mirror = match args.get(2) {
        Some(Value::Object(Some(m))) => *m,
        _ => return Err(mh_null_array_type()),
    };
    let len = match args.get(3) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let array_desc = mh_mirror_descriptor_strict(&*ctx, args.get(2));
    let tdesc = mh_type_descriptor(ctx, target);
    let described = match (array_desc, tdesc) {
        (Some(a), Some(t)) => split_descriptor_params(&t).map(|(params, _)| (a, t, params)),
        _ => None,
    };
    let Some((array_desc, tdesc, params)) = described else {
        // Undescribable: the trailing form, the only one there was.
        return ctx.invoke_virtual(
            target,
            "asCollector",
            MH_AS_COLLECTOR_DESC,
            &[Value::Object(Some(array_mirror)), Value::Int(len)],
        );
    };
    if let Some(refusal) = mh_collector_refusal(&*ctx, &tdesc, &array_desc, i64::from(pos), len) {
        return Err(refusal);
    }
    let pos_u = pos as usize;
    if pos_u + 1 == params.len() {
        return ctx.invoke_virtual(
            target,
            "asCollector",
            MH_AS_COLLECTOR_DESC,
            &[Value::Object(Some(array_mirror)), Value::Int(len)],
        );
    }
    let param = params[pos_u].clone();
    let target_pin = ctx.pin_native_root(target);
    let mirror_pin = ctx.pin_native_root(array_mirror);
    let result = mh_collect_at(
        ctx,
        target_pin,
        target,
        mirror_pin,
        array_mirror,
        pos,
        len,
        &array_desc,
        &param,
    );
    ctx.unpin_native_roots(target_pin);
    result
}

/// `CRATONVM_MH_NATIVE_COMBINATOR_DIRECT` (default on, both modes; round 15
/// wave 2, lane vh2): a native that composes a `MethodHandles` combinator
/// calls the combinator's registered Rust body instead of `invoke_static` by
/// name ([`mh_collect_at`]). `0` restores the by-name call, which under
/// `--jdk-only` runs the JDK body (`NoSuchMethodError: MethodHandle.editor()`).
pub(crate) fn mh_native_combinator_direct_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_NATIVE_COMBINATOR_DIRECT")
    })
}

/// The middle-position half of [`mh_as_collector_at`]; `target` and the
/// array mirror are pinned by the caller (re-read here after each call).
#[allow(clippy::too_many_arguments)]
pub(crate) fn mh_collect_at(
    ctx: &mut dyn NativeContext,
    target_pin: usize,
    target: ObjectRef,
    mirror_pin: usize,
    array_mirror: ObjectRef,
    pos: i32,
    len: i32,
    array_desc: &str,
    param: &str,
) -> MethodCallResult {
    let identity = ctx.invoke_static(
        "java/lang/invoke/MethodHandles",
        "identity",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(array_mirror))],
    )?;
    let identity = mh_handle_answer(identity, "identity")?;
    let array_mirror = ctx.read_native_pin(mirror_pin, array_mirror);
    let collector = ctx.invoke_virtual(
        identity,
        "asCollector",
        MH_AS_COLLECTOR_DESC,
        &[Value::Object(Some(array_mirror)), Value::Int(len)],
    )?;
    let mut collector = mh_handle_answer(collector, "asCollector")?;
    if param != array_desc {
        let component = &array_desc[1..];
        let desc = format!("({}){param}", component.repeat(len.max(0) as usize));
        collector = mh_as_type_to_desc(ctx, collector, &desc)?;
    }
    let target = ctx.read_native_pin(target_pin, target);
    // Round 15 wave 2 (lane vh2;
    // `r15w1-vh-native-to-java-combinator-calls-under-jdk-only-FIXED-20260930.md`):
    // the registered `collectArguments` body, called directly. Reached by
    // name from a native, the combinator takes the class-shared door, where
    // `--jdk-only` lets the JDK body (`target.rebind().editor()`) beat its
    // `Bridge`. `identity` above is pinned by `check_override` on every door.
    // `collect_arguments_refusal` is not asked: it judges only direct handles,
    // and `collector` is an `asCollector` adapter.
    if mh_native_combinator_direct_enabled() {
        return make_collect_args_adapter(
            ctx,
            Some(Value::Object(Some(target))),
            pos,
            Some(Value::Object(Some(collector))),
        );
    }
    ctx.invoke_static(
        "java/lang/invoke/MethodHandles",
        "collectArguments",
        "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        &[
            Value::Object(Some(target)),
            Value::Int(pos),
            Value::Object(Some(collector)),
        ],
    )
}

/// JDK 25 `MethodType.asSpreaderType(arrayType, pos, arrayLength)` over a
/// descriptor, including its `Object[]`-at-position-0 shortcut
/// (`genericMethodType(arrayLength)` with the return kept), which drops every
/// parameter past the spread; `asType` then refuses the arity change
/// (`L4W44CombinatorHunt` row `asSpreader-pos`, measured on HotSpot 25).
pub(crate) fn mh_spreader_need_type(
    params: &[String],
    ret: &str,
    array_desc: &str,
    pos: usize,
    len: usize,
) -> String {
    const OBJECT: &str = "Ljava/lang/Object;";
    let own = format!("({}){ret}", params.concat());
    if len == 0 {
        return own;
    }
    if array_desc == "[Ljava/lang/Object;" {
        if ret == OBJECT && params.iter().all(|p| p == OBJECT) {
            return own;
        }
        if pos == 0 {
            return format!("({}){ret}", OBJECT.repeat(len));
        }
    }
    let elem = &array_desc[1..];
    match params.iter().skip(pos).take(len).position(|p| p != elem) {
        Some(offset) => {
            let mut fixed = params.to_vec();
            for p in fixed.iter_mut().take(pos + len).skip(pos + offset) {
                *p = elem.to_string();
            }
            format!("({}){ret}", fixed.concat())
        }
        None => own,
    }
}

/// `MethodHandle.asSpreader(int spreadArgPos, Class arrayType, int
/// arrayLength)`. The JDK body ends in `rebind().editor()`, which a CratonVM
/// handle has not (`NoSuchMethodError`). `asSpreaderChecks` first
/// (`spreadArrayChecks`, `bad spread array length`, `bad spread position`,
/// then `asType` to the spread-compatible type, whose refusal is the JDK's
/// error); then the two-argument `asSpreader` native at the trailing
/// position, and elsewhere that native between two `permuteArguments` that
/// move the spread parameters to the end and the array back to `pos`.
pub(crate) fn mh_as_spreader_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(answer) = mh_foreign_adapter_through_jdk(
        ctx,
        args,
        "asSpreader",
        "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;",
    ) {
        return answer;
    }
    let target = obj_arg(args, 0)?;
    let pos = match args.get(1) {
        Some(Value::Int(p)) => *p,
        _ => 0,
    };
    let array_mirror = match args.get(2) {
        Some(Value::Object(Some(m))) => *m,
        _ => return Err(mh_null_array_type()),
    };
    let len = match args.get(3) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let array_desc = mh_mirror_descriptor_strict(&*ctx, args.get(2));
    let tdesc = mh_type_descriptor(ctx, target);
    let described = match (array_desc, tdesc) {
        (Some(a), Some(t)) => split_descriptor_params(&t).map(|(params, ret)| (a, t, params, ret)),
        _ => None,
    };
    let Some((array_desc, tdesc, params, ret)) = described else {
        return ctx.invoke_virtual(
            target,
            "asSpreader",
            MH_AS_SPREADER_DESC,
            &[Value::Object(Some(array_mirror)), Value::Int(len)],
        );
    };
    if let Some(refusal) = mh_spread_array_refusal(&*ctx, &array_desc, len) {
        return Err(refusal);
    }
    let nargs = params.len() as i64;
    if nargs < i64::from(len) || len < 0 {
        return Err(mh_illegal_argument("bad spread array length".to_string()));
    }
    if pos < 0 || i64::from(pos) + i64::from(len) > nargs {
        return Err(mh_illegal_argument("bad spread position".to_string()));
    }
    let (pos_u, len_u) = (pos as usize, len as usize);
    let need = mh_spreader_need_type(&params, &ret, &array_desc, pos_u, len_u);
    let mirror_pin = ctx.pin_native_root(array_mirror);
    let result = mh_spread_at(
        ctx,
        target,
        &tdesc,
        &need,
        &ret,
        mirror_pin,
        array_mirror,
        pos_u,
        len_u,
        &array_desc,
    );
    ctx.unpin_native_roots(mirror_pin);
    result
}

/// The half of [`mh_as_spreader_at`] after its checks; the array mirror is
/// pinned by the caller.
#[allow(clippy::too_many_arguments)]
pub(crate) fn mh_spread_at(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    tdesc: &str,
    need: &str,
    ret: &str,
    mirror_pin: usize,
    array_mirror: ObjectRef,
    pos: usize,
    len: usize,
    array_desc: &str,
) -> MethodCallResult {
    let Some((need_params, _)) = split_descriptor_params(need) else {
        return mh_handle_answer(None, "asSpreader's spread type")
            .map(|mh| Some(Value::Object(Some(mh))));
    };
    let k = need_params.len();
    // SBR2 W9 MHFIX: at the trailing position the spread keeps the ORIGINAL
    // target: its `MH_KIND_SPREAD` arm converts each element to the target's
    // own parameter types before the target runs (outside a `catchException`
    // the target is, as `Proxy.defaultMethodHandle` builds it), where an
    // `asType` copy to `need` kept the conversion in the leaf, inside the
    // catch. The `asType` refusal is still judged.
    let trailing = pos + len == k;
    let after = if need != tdesc {
        if let Some(refusal) = mh_astype_refusal_desc(ctx, target, tdesc, need) {
            return Err(refusal);
        }
        if trailing {
            target
        } else {
            mh_as_type_to_desc(ctx, target, need)?
        }
    } else {
        target
    };
    let array_mirror = ctx.read_native_pin(mirror_pin, array_mirror);
    if pos + len == k {
        return ctx.invoke_virtual(
            after,
            "asSpreader",
            MH_AS_SPREADER_DESC,
            &[Value::Object(Some(array_mirror)), Value::Int(len as i32)],
        );
    }
    // Spread parameters moved to the end: (before, after, spread)R.
    let mut moved: Vec<String> = Vec::with_capacity(k);
    moved.extend_from_slice(&need_params[..pos]);
    moved.extend_from_slice(&need_params[pos + len..]);
    moved.extend_from_slice(&need_params[pos..pos + len]);
    let reorder_in: Vec<i32> = (0..k)
        .map(|i| {
            if i < pos {
                i as i32
            } else if i < pos + len {
                (k - len + (i - pos)) as i32
            } else {
                (i - len) as i32
            }
        })
        .collect();
    let moved_mh = mh_permute_to_desc(
        ctx,
        after,
        &format!("({}){ret}", moved.concat()),
        &reorder_in,
    )?;
    let array_mirror = ctx.read_native_pin(mirror_pin, array_mirror);
    let spread = ctx.invoke_virtual(
        moved_mh,
        "asSpreader",
        MH_AS_SPREADER_DESC,
        &[Value::Object(Some(array_mirror)), Value::Int(len as i32)],
    )?;
    let spread = mh_handle_answer(spread, "asSpreader")?;
    // The array back at `pos`: (before, array, after)R.
    let mut back: Vec<String> = Vec::with_capacity(k - len + 1);
    back.extend_from_slice(&need_params[..pos]);
    back.push(array_desc.to_string());
    back.extend_from_slice(&need_params[pos + len..]);
    let reorder_out: Vec<i32> = (0..(k - len + 1))
        .map(|j| {
            if j < pos {
                j as i32
            } else if j < k - len {
                (j + 1) as i32
            } else {
                pos as i32
            }
        })
        .collect();
    let result = mh_permute_to_desc(
        ctx,
        spread,
        &format!("({}){ret}", back.concat()),
        &reorder_out,
    )?;
    Ok(Some(Value::Object(Some(result))))
}

/// `MethodHandles.dropReturn(target)` (JDK 25): the target itself for a
/// `void` return, else the target adapted to the same parameters and
/// `void`, which is `asType`'s return rule ("the return value is
/// discarded"). The JDK body builds a `filterReturnForm` with
/// `rebind().editor()` (`NoSuchMethodError` on a CratonVM handle;
/// `L4W44CombinatorHunt` row `countedLoop-void`).
pub(crate) fn mhs_drop_return(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let target = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Err(mh_factory_null_argument(None)),
    };
    let Some((params, ret)) =
        mh_type_descriptor(ctx, target).and_then(|d| split_descriptor_params(&d))
    else {
        return Ok(Some(Value::Object(Some(target))));
    };
    if ret == DESC_VOID {
        return Ok(Some(Value::Object(Some(target))));
    }
    let dropped = mh_as_type_to_desc(ctx, target, &format!("({})V", params.concat()))?;
    Ok(Some(Value::Object(Some(dropped))))
}

/// Derive a loop adapter's `type()` from its `body` handle's descriptor,
/// dropping `drop_leading` leading parameters (the loop-local accumulator
/// plus, for `countedLoop`/`iteratedLoop`, the counter/item slot) and
/// optionally prefixing one leading parameter with no counterpart in
/// `body`'s own signature (`iteratedLoop`'s `Iterable`). Not exercised by any
/// `.type()` assertion in the sweep this fixes
/// (`lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md` names none), so a
/// descriptor `split_descriptor_params` can't parse degrades to a safe,

/// arity-erased `()Ljava/lang/Object;` rather than failing the factory call.
pub(crate) fn make_loop_type_desc(
    ctx: &mut dyn NativeContext,
    body: ObjectRef,
    drop_leading: usize,
    prefix_param: Option<&str>,
) -> String {
    let bdesc = mh_type_descriptor(ctx, body)
        .or_else(|| mh_read_desc(ctx, body))
        .unwrap_or_default();
    match split_descriptor_params(&bdesc) {
        Some((params, ret)) => {
            // A `void` body has no loop-variable parameter to drop (wave 26,
            // `loop_variable_shape`): `countedLoop`'s is `(int i, A...)void`.
            let drop_leading = if ret == "V" {
                drop_leading.saturating_sub(1)
            } else {
                drop_leading
            };
            let extra: String = params.into_iter().skip(drop_leading).collect();
            let prefix = prefix_param.unwrap_or("");
            format!("({prefix}{extra}){ret}")
        }
        None => "()Ljava/lang/Object;".to_string(),
    }
}

/// Shared body of `countedLoop`/`whileLoop`/`doWhileLoop`'s factories — all
/// three take exactly 3 `MethodHandle` arguments and store them in a 3-slot
/// carrier VERBATIM, in their own call's argument order (the dispatch side
/// for each `kind` knows which slot means what — see `MH_KIND_LOOP_COUNTED`/
/// `_WHILE`/`_DOWHILE`'s doc comments). `body_slot` is which of `args[0..3]`
/// is the `body` handle, for `type()` derivation.
pub(crate) fn make_loop3_adapter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    kind: i32,
    body_slot: usize,
    drop_leading: usize,
) -> MethodCallResult {
    let a0 = args.first().copied().unwrap_or(Value::Object(None));
    let a1 = args.get(1).copied().unwrap_or(Value::Object(None));
    let a2 = args.get(2).copied().unwrap_or(Value::Object(None));
    let slots = [a0, a1, a2];
    let body_mh = match slots.get(body_slot).copied() {
        Some(Value::Object(Some(m))) => m,
        _ => return Ok(Some(Value::Object(None))),
    };
    // gc-common w20-f: the descriptor is read from `body_mh` before the
    // carrier's allocation, and the three handles are stored at their current
    // addresses (both used to happen after it, at pre-allocation addresses).
    let desc = make_loop_type_desc(ctx, body_mh, drop_leading, None);
    let wrapper = alloc_mh_carrier_holding(ctx, "__mh_loop3_wrapper__", &[a0, a1, a2]);
    // gen r4w3/rooting: `wrapper` is stored after `alloc_method_handle`
    // (a GC point); pin and re-read.
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let adapter: ObjectRef = alloc_method_handle(ctx, "__adapter__", "loop", &desc, kind)?;
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    ctx.unpin_native_roots(wrapper_pin);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    Ok(Some(Value::Object(Some(adapter))))
}

/// Functional `MethodHandles.countedLoop`/`whileLoop`/`doWhileLoop`/
/// `iteratedLoop`/`tryFinally` — the whole "a JDK 9 API absent" bucket from
/// `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`.
/// Real JDK bytecode for all five builds `LambdaForm`/

/// `BoundMethodHandle` machinery this VM's `MH_KIND_*` shim model does not
/// have (the same species of gap `constant`/`identity`/`insertArguments`
/// already work around) — without a native every one of them either NPEs,
/// throws `NoSuchMethodError`, or, once an earlier failure has left
/// `BoundMethodHandle`'s `<clinit>` in the JVMS 5.5 ERRONEOUS state, throws
/// `NoClassDefFoundError`. Promoted to the real-JDK essentials path (shared
/// with the synthetic-jdk registration) and `check_override`-allow-listed in
/// `vm/src/vm/vm_exec.rs`, mirroring every other combinator bridge in this
/// file.
pub(crate) fn register_method_handle_loop_bridge(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mh = "java/lang/invoke/MethodHandles";
    let desc3 = "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;";

    // countedLoop(iterations, init, body): body is args[2], dropping its
    // leading (v, i) pair for type().
    r.register(mh, "countedLoop", desc3, |ctx, args| {
        // `countedLoop(empty(iterations.type()), iterations, init, body)`:
        // `iterations.type()` is the first dereference (a helpful NPE), then
        // `countedLoopChecks` (wave 44).
        let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Object(None));
        let (iterations, init, body) = (arg(0), arg(1), arg(2));
        let Value::Object(Some(iterations_mh)) = iterations else {
            return Err(mh_factory_null_argument(Some(
                "Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"iterations\" is null",
            )));
        };
        let Value::Object(Some(body_mh)) = body else {
            return Err(mh_factory_null_argument(None));
        };
        let desc = counted_loop_type_desc(ctx, iterations_mh, iterations_mh, init, body_mh)?;
        let wrapper = alloc_mh_carrier_holding(ctx, "__mh_loop3_wrapper__", &[iterations, init, body]);
        let wrapper_pin = ctx.pin_native_root(wrapper);
        let adapter: ObjectRef =
            alloc_method_handle(ctx, "__adapter__", "loop", &desc, MH_KIND_LOOP_COUNTED)?;
        let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
        ctx.unpin_native_roots(wrapper_pin);
        ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
        Ok(Some(Value::Object(Some(adapter))))
    });
    // whileLoop(init, pred, body): body is args[2], dropping its leading v.
    r.register(mh, "whileLoop", desc3, |ctx, args| {
        let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Object(None));
        if let Some(refusal) = mh_while_loop_refusal(ctx, arg(0), arg(1), arg(2)) {
            return Err(refusal);
        }
        make_loop3_adapter(ctx, args, MH_KIND_LOOP_WHILE, 2, 1)
    });
    // doWhileLoop(init, body, pred): body is args[1] — the ARGUMENT ORDER
    // that makes `MH_KIND_LOOP_DOWHILE` read a different wrapper layout than
    // `MH_KIND_LOOP_WHILE`, though the carrier is stored verbatim either way.
    r.register(mh, "doWhileLoop", desc3, |ctx, args| {
        // `doWhileLoop(init, body, pred)`: the checks take `(init, pred, body)`.
        let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Object(None));
        if let Some(refusal) = mh_while_loop_refusal(ctx, arg(0), arg(2), arg(1)) {
            return Err(refusal);
        }
        make_loop3_adapter(ctx, args, MH_KIND_LOOP_DOWHILE, 1, 1)
    });
    // iteratedLoop(iterator, init, body): the carrier holds (init, body,
    // iterator); see `MH_KIND_LOOP_ITERATE`'s doc comment.
    r.register(mh, "iteratedLoop", desc3, |ctx, args| {
        let init = args.get(1).copied().unwrap_or(Value::Object(None));
        // `iteratedLoopChecks`' first step is `Objects.requireNonNull(body)`:
        // a message-less NPE (interpreter round i5 wave 106, lane L4,
        // `L4W106LoopFactoryNulls`; this answered a null handle).
        let body_mh = match args.get(2) {
            Some(Value::Object(Some(m))) => *m,
            _ => return Err(mh_factory_null_argument(None)),
        };
        // Interpreter round i7 wave 141, lane L4
        // (`CRATONVM_MH_ITERATED_LOOP_CHECKS`, parity, default on): the rest
        // of `iteratedLoopChecks`, before anything is built.
        if mh_iterated_loop_checks_enabled() {
            let iterator = args.first().copied().unwrap_or(Value::Object(None));
            if let Some(refusal) = mh_iterated_loop_refusal(ctx, iterator, init, body_mh) {
                return Err(refusal);
            }
        }
        // gc-common w20-f: descriptor first, handles stored at their current
        // addresses (see `make_loop3_adapter`).
        // The loop's parameters are the body's `A...` after `(V T)`; only a
        // body with no `A` gets the default iterator's `Iterable` (JDK 25
        // `iteratedLoop`: "if the body contributes only V and T types ... the
        // single type Iterable is added"). Wave 44, lane L4
        // (`L4W44CombinatorHunt` row `iteratedLoop`): it was always added, so
        // a body `(V T Iterable)V` made a `(Iterable,Iterable)V` loop.
        // A non-null `iterator` contributes its own parameters instead
        // (same rule), and is kept in the carrier's slot 2 (wave 44: it was
        // dropped, and the loop walked its first argument as an `Iterable`).
        let iterator = args.first().copied().unwrap_or(Value::Object(None));
        let own = make_loop_type_desc(ctx, body_mh, 2, None);
        let desc = match (own.strip_prefix("()"), iterator) {
            (Some(ret), Value::Object(Some(it))) => match mh_type_descriptor(ctx, it)
                .as_deref()
                .and_then(split_descriptor_params)
            {
                Some((params, _)) => format!("({}){ret}", params.concat()),
                None => own.clone(),
            },
            (Some(_), _) => make_loop_type_desc(ctx, body_mh, 2, Some("Ljava/lang/Iterable;")),
            (None, _) => own.clone(),
        };
        let wrapper = alloc_mh_carrier_holding(
            ctx,
            "__mh_iterate_wrapper__",
            &[init, Value::Object(Some(body_mh)), iterator],
        );
        // gen r4w3/rooting: `wrapper` is stored after `alloc_method_handle`
        // (a GC point); pin and re-read.
        let wrapper_pin = ctx.pin_native_root(wrapper);
        let adapter: ObjectRef =
            alloc_method_handle(ctx, "__adapter__", "iterate", &desc, MH_KIND_LOOP_ITERATE)?;
        let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
        ctx.unpin_native_roots(wrapper_pin);
        ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
        Ok(Some(Value::Object(Some(adapter))))
    });
    r.register(
        mh,
        "tryFinally",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // JDK 25 `tryFinally` dereferences `target.type()` first, then
            // `tryFinallyChecks` dereferences `cleanup.type()`: helpful NPEs
            // naming each (interpreter round i5 wave 106, lane L4,
            // `L4W106LoopFactoryNulls`; a null target answered a null handle
            // and a null cleanup built an adapter that failed when invoked).
            let target = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => {
                    return Err(mh_factory_null_argument(Some(
                        "Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"target\" is null",
                    )))
                }
            };
            let cleanup = args.get(1).copied().unwrap_or(Value::Object(None));
            if matches!(cleanup, Value::Object(None)) {
                return Err(mh_factory_null_argument(Some(
                    "Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"cleanup\" is null",
                )));
            }
            // `MethodHandles.tryFinallyChecks` (wave 23): the four type
            // refusals, which were missing.
            if let Value::Object(Some(cleanup_mh)) = cleanup {
                if let (Some(tdesc), Some(cdesc)) =
                    (mh_type_descriptor(ctx, target), mh_type_descriptor(ctx, cleanup_mh))
                {
                    if let Some(refusal) = mh_try_finally_refusal(&*ctx, &tdesc, &cdesc) {
                        return Err(refusal);
                    }
                }
            }
            // type(): tryFinally does not change the caller-visible signature
            // at all — the adapter's type is the target's, verbatim.
            //
            // gc-common w20-f: read before the carrier's allocation, and the
            // handles stored at their current addresses (see
            // `make_loop3_adapter`).
            let desc = mh_type_descriptor(ctx, target)
                .or_else(|| mh_read_desc(ctx, target))
                .unwrap_or_default();
            let wrapper = alloc_mh_carrier_holding(
                ctx,
                "__mh_tryfinally_wrapper__",
                &[Value::Object(Some(target)), cleanup],
            );
            // gen r4w3/rooting: `wrapper` is stored after
            // `alloc_method_handle` (a GC point); pin and re-read.
            let wrapper_pin = ctx.pin_native_root(wrapper);
            let adapter: ObjectRef =
                alloc_method_handle(ctx, "__adapter__", "tryfinally", &desc, MH_KIND_TRY_FINALLY)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            ctx.unpin_native_roots(wrapper_pin);
            ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
            Ok(Some(Value::Object(Some(adapter))))
        },
    );
    // countedLoop(start, end, init, body) (interpreter round i1 wave 44, lane
    // L4): the four-handle form had no native, so its JDK body ran the
    // `LambdaForm` loop machinery the three-handle form is pinned away from.
    // The carrier is `(end, init, body, start)`: the three-handle layout plus
    // a fourth slot `mh_dispatch_counted_loop` reads as the first index.
    r.register(
        mh,
        "countedLoop",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Object(None));
            let (start, end, init, body) = (arg(0), arg(1), arg(2), arg(3));
            // `countedLoopChecks`' `Objects.requireNonNull(start / end / body)`.
            let (start_mh, end_mh, body_mh) = match (start, end, body) {
                (Value::Object(Some(s)), Value::Object(Some(e)), Value::Object(Some(b))) => (s, e, b),
                _ => return Err(mh_factory_null_argument(None)),
            };
            let desc = counted_loop_type_desc(ctx, start_mh, end_mh, init, body_mh)?;
            let wrapper =
                alloc_mh_carrier_holding(ctx, "__mh_loop4_wrapper__", &[end, init, body, start]);
            let wrapper_pin = ctx.pin_native_root(wrapper);
            let adapter: ObjectRef =
                alloc_method_handle(ctx, "__adapter__", "loop", &desc, MH_KIND_LOOP_COUNTED)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            ctx.unpin_native_roots(wrapper_pin);
            ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
            Ok(Some(Value::Object(Some(adapter))))
        },
    );
    r.register(
        mh,
        "tableSwitch",
        "(Ljava/lang/invoke/MethodHandle;[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        make_table_switch_adapter,
    );
    r.set_category(__prev_cat);
}

/// `MethodHandles.tableSwitch(fallback, targets...)` (interpreter round i1
/// wave 44, lane L4; `L4W44CombinatorHunt`, rows `tableSwitch*`). JDK 25
/// builds it with `MethodHandleImpl.makeTableSwitch`, a `BoundMethodHandle`
/// species over a spun `LambdaForm` (`speciesData_LLLL`), which the
/// `MH_KIND_*` model cannot run, and its `tableSwitchChecks` compare
/// `MethodType`s by identity. This native makes the same checks in the JDK's
/// order and texts (`Objects.requireNonNull` of `fallback`, `targets` and each
/// target; `Not enough cases: [...]`; `Case actions must have int as leading
/// parameter: [...]`; `Case actions must have the same type: [...]`, each
/// `[...]` the targets' `MethodHandle.toString()`), then answers an
/// `MH_KIND_TABLE_SWITCH` handle of the fallback's type whose carrier is
/// `(N, fallback, targets...)`, a copy (the JDK clones `targets`).
pub(crate) fn make_table_switch_adapter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fallback = match args.first() {
        Some(Value::Object(Some(f))) => *f,
        _ => return Err(mh_factory_null_argument(None)),
    };
    let targets_arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(mh_factory_null_argument(None)),
    };
    let mut slots: Vec<Value> = Vec::with_capacity(1 + ctx.array_length(targets_arr));
    slots.push(Value::Object(Some(fallback)));
    for i in 0..ctx.array_length(targets_arr) {
        slots.push(ctx.get_array_element(targets_arr, i));
    }
    // The texts are rendered before any allocation below (a GC point for
    // every reference in `slots`).
    if slots.len() == 1 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Not enough cases: []".to_string(),
        }
        .into());
    }
    let expected = mh_declared_descriptor(ctx, fallback);
    if let Some(expected) = expected.as_deref() {
        if !expected.starts_with("(I") {
            let text = table_switch_cases_text(ctx, &slots[1..]);
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Case actions must have int as leading parameter: {text}"),
            }
            .into());
        }
    }
    for case in &slots[1..] {
        let Value::Object(Some(h)) = case else {
            return Err(mh_factory_null_argument(None));
        };
        let declared = mh_declared_descriptor(ctx, *h);
        if let (Some(expected), Some(declared)) = (expected.as_deref(), declared.as_deref()) {
            if declared != expected {
                let text = table_switch_cases_text(ctx, &slots[1..]);
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("Case actions must have the same type: {text}"),
                }
                .into());
            }
        }
    }
    let desc = expected
        .or_else(|| mh_type_descriptor(ctx, fallback))
        .unwrap_or_default();
    // `(case count, fallback, targets...)`: see `MH_KIND_TABLE_SWITCH`.
    let mut carrier: Vec<Value> = Vec::with_capacity(slots.len() + 1);
    carrier.push(Value::Int((slots.len() - 1) as i32));
    carrier.extend_from_slice(&slots);
    let wrapper = alloc_mh_carrier_holding(ctx, "__mh_table_switch_wrapper__", &carrier);
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let adapter: ObjectRef = alloc_method_handle(
        ctx,
        "__adapter__",
        "tableSwitch",
        &desc,
        MH_KIND_TABLE_SWITCH,
    )?;
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    ctx.unpin_native_roots(wrapper_pin);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    Ok(Some(Value::Object(Some(adapter))))
}

/// `Arrays.toString(caseActions)` for [`make_table_switch_adapter`]'s
/// refusals: each handle's `MethodHandle.toString()` (`MethodHandle` and its
/// type), `null` for a null element.
pub(crate) fn table_switch_cases_text(ctx: &mut dyn NativeContext, cases: &[Value]) -> String {
    let mut shown: Vec<String> = Vec::with_capacity(cases.len());
    for case in cases {
        shown.push(match case {
            Value::Object(Some(h)) => {
                let declared = mh_declared_descriptor(ctx, *h);
                match declared.and_then(|d| method_type_display_in(&*ctx, &d)) {
                    Some(t) => format!("MethodHandle{t}"),
                    None => "MethodHandle".to_string(),
                }
            }
            _ => "null".to_string(),
        });
    }
    format!("[{}]", shown.join(", "))
}

/// Read a call site's current target MethodHandle descriptor (real
/// `CallSite.target` field, falling back to the synthetic CallSite model's
/// slot 0), for stamping a dynamic-invoker handle's descriptor.
///
/// The target's `type()` -- `CallSite.type()`, which `setTarget` holds every
/// target to -- not its leaf's `MH_DESC`: an `asType` copy of an adapter
/// (`insertArguments(add, 0, 5).asType((Object)Object)`, Groovy's call-site
/// shape) answered `(int,int)int`, so `dynamicInvoker().type()` was the
/// leaf's and its `asType(genericMethodType(1))` a `WrongMethodTypeException`
/// (HotSpot 25.0.3: `(Object)Object`; interpreter round i2 wave 59, lane L4,
/// `L4W59CombinatorCallSiteTypes` rows `insertArguments`, `foldArguments`,
/// `asCollector`, `asSpreader`). `CRATONVM_MH_DYNAMIC_INVOKER_SITE_TYPE=0`
/// restores the leaf's descriptor.
pub(crate) fn callsite_target_desc(ctx: &mut dyn NativeContext, callsite: ObjectRef) -> Option<String> {
    let target = match cs_read_target(ctx, callsite) {
        Value::Object(Some(t)) => t,
        _ => return None,
    };
    if crate::nbflags().mh_dynamic_invoker_site_type {
        return mh_type_descriptor(ctx, target);
    }
    mh_read_desc(ctx, target)
}

pub(crate) fn register_p65_method_handles_extra(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mh = "java/lang/invoke/MethodHandles";
    register_array_element_accessor_bridges(r);
    // Functional `constant`/`identity` shims (shared with the real-JDK
    // essentials path).
    register_method_handles_constant_bridge(r);
    register_method_handles_identity_bridge(r);
    // Functional `insertArguments`/`asCollector` + CallSite-construction
    // natives (shared with the real-JDK essentials path).
    register_method_handle_combinator_extras_bridge(r);
    // Functional loop family + `tryFinally` (shared with the real-JDK
    // essentials path).
    register_method_handle_loop_bridge(r);
    r.register(
        mh,
        "dropArguments",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        make_drop_arguments_adapter,
    );
    // Functional `empty`/`zero` shims (shared with the real-JDK essentials
    // path).
    register_method_handles_zero_empty_bridge(r);
    r.set_category(__prev_cat);
}

/// Zero/default `Value` for a primitive return descriptor, or `Object(None)`
/// (null) for anything else — shared by `zero`/`empty` below.
pub(crate) fn mh_zero_value_for_descriptor(desc: &str) -> Value {
    match desc.as_bytes().first() {
        Some(b'J') => Value::Long(0),
        Some(b'F') => Value::Float(0.0),
        Some(b'D') => Value::Double(0.0),
        Some(b'Z' | b'B' | b'C' | b'S' | b'I') => Value::Int(0),
        _ => Value::Object(None),
    }
}

/// `MethodHandles.zero(Class)` / `MethodHandles.empty(MethodType)` — functional
/// `MH_KIND_CONSTANT` shims (`zero` is exactly `constant(type, <the type's
/// default value>)`; `empty` takes an arbitrary `MethodType` rather than a
/// bare return `Class`, but `MH_KIND_CONSTANT`'s dispatch already ignores its
/// incoming arguments entirely, so the SAME kind serves both).
///
/// Previously both were INERT: a bare 17-slot `MethodHandle` with a `type`
/// field and no `MH_KIND`/`MH_BOUND` at all — allocated directly rather than
/// through `alloc_method_handle`, the exact species of bug
/// `MH_KIND_ARRAY_GET`'s doc comment records for the array accessors. That
/// happened to read back `null` for a REFERENCE `zero`, which is the right
/// answer by accident (source doc's "one row that was passing by accident"),
/// and wrong for every primitive one — `k.zeroInt` was measured
/// `NullPointerException` where HotSpot answers `0`.
///
/// Worse, and the reason this pair is fixed alongside the loop family rather
/// than left alone: this registrar was ONLY ever reachable from
/// `register_p65_method_handles_extra`, itself only called from
/// `register_phase65_natives`, itself only called from the `#[cfg(feature =
/// "synthetic-jdk")]`-gated `register_synthetic_overrides` — the exact
/// wiring bug `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md` §"Why the
/// first attempt failed" describes for the ORIGINAL loop-family attempt, but
/// pre-existing here rather than newly introduced. A real-JDK CLI build
/// (the default; no `synthetic-jdk` feature) never registered `zero`/`empty`
/// at all, so real `MethodHandles.zero`/`empty` bytecode ran instead — both
/// call `MethodHandleImpl.makeConstantReturning` DIRECTLY (not through the
/// public `MethodHandles.constant`, so the already-shimmed `constant` native
/// never intercepts it), which spins the `BoundMethodHandle`/
/// `ClassSpecializer` species machinery this VM's shim model can't run:
/// measured `InternalError: Failed to link speciesData to speciesCode`
/// isolated, and `NoClassDefFoundError` once an earlier call has left
/// `BoundMethodHandle`'s `<clinit>` in the JVMS 5.5 ERRONEOUS state (which is
/// why `k.zeroRef`/`k.emptyVoid` — reference-typed, "passing by accident"
/// under the old inert stub — broke too, not only `k.zeroInt`).
pub(crate) fn register_method_handles_zero_empty_bridge(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mh = "java/lang/invoke/MethodHandles";
    r.register(
        mh,
        "zero",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let ret_desc = match args.first() {
                Some(Value::Object(Some(m))) => mirror_to_descriptor(ctx, *m).into_owned(),
                // `zero(null)`: HotSpot's message-less NPE (wave 40).
                Some(Value::Object(None)) => return Err(mh_factory_null_argument(None)),
                _ => DESC_OBJECT.to_string(),
            };
            let desc = format!("(){ret_desc}");
            let handle = alloc_method_handle(ctx, "", "", &desc, MH_KIND_CONSTANT)?;
            ctx.set_field(handle, MH_BOUND, mh_zero_value_for_descriptor(&ret_desc));
            Ok(Some(Value::Object(Some(handle))))
        },
    );
    r.register(
        mh,
        "empty",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let mt = match args.first() {
                Some(Value::Object(Some(m))) => Some(*m),
                // `empty(null)`: HotSpot's message-less NPE (wave 40).
                Some(Value::Object(None)) => return Err(mh_factory_null_argument(None)),
                _ => None,
            };
            let desc = mt
                .and_then(|m| methodtype_to_descriptor(ctx, m))
                .unwrap_or_else(|| "()V".to_string());
            let ret_desc = return_type_desc(&desc).to_string();
            let handle = alloc_method_handle(ctx, "", "", &desc, MH_KIND_CONSTANT)?;
            ctx.set_field(handle, MH_BOUND, mh_zero_value_for_descriptor(&ret_desc));
            Ok(Some(Value::Object(Some(handle))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.lang.invoke extras — MethodHandleProxies, LambdaMetafactory
// =============================================================================

pub fn register_p68_invoke_extras(r: &mut NativeMethodRegistry) {
    // Mixed block:
    //   * MethodHandleProxies.asInterfaceInstance is a SIMPLIFIED stub — it
    //     returns the MH itself as the "proxy" — and `isWrapperInstance` /
    //     `wrapperInstanceTarget` are answered CONSISTENTLY with that
    //     (is-a-MethodHandle / the handle itself) instead of the old blanket
    //     0/null, which contradicted what `asInterfaceInstance` had just
    //     returned. `wrapperInstanceType` is not registered — see below.
    //     SINCE 2026-08-21 (H19) all three are registered ONLY when this
    //     registry is NOT being populated for a real JDK image: against a real
    //     image the simplification is a `ClassCastException` at every call
    //     site and the JDK's own bytecode is measured correct. See the guard.
    //   * LambdaMetafactory.metafactory / altMetafactory are now a faithful
    //     bridge to the VM's real lambda-proxy machinery (the same one the
    //     `invokedynamic` opcode uses): they register a proxy class via
    //     `register_lambda_proxy` and hand back a FROZEN ConstantCallSite whose
    //     target is an `MH_KIND_LAMBDA_FACTORY` MethodHandle. Invoking that
    //     target with the captured args yields a genuine SAM instance whose
    //     abstract method dispatches to the impl method — so reflective callers
    //     (log4j2 `ServiceLoaderUtil`, Elasticsearch CLI bootstrap) work end to
    //     end. A no-op frozen CallSite remains only as a defensive fallback when
    //     the implMethod is absent.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // MethodHandleProxies
    let mhp = "java/lang/invoke/MethodHandleProxies";
    // NOT REGISTERED WHEN A REAL JDK IMAGE IS LOADED (H19, 2026-08-21).
    //
    // The three registrations below are the `asInterfaceInstance`
    // simplification and the two queries that were made consistent with it.
    // They are correct only in the synthetic-JDK image, where
    // `MethodHandleProxies` has no bytecode at all. Against a real image they
    // are the entire defect: `asInterfaceInstance` reads `args[1]` and IGNORES
    // `args[0]` (the interface), so every call site got the `MethodHandle`
    // itself and threw
    // `ClassCastException: java.lang.invoke.MethodHandle cannot be cast to
    // <the requested interface>`.
    //
    // The historical reason for the simplification is GONE. The real
    // `MethodHandleProxies` spins a proxy whose `<init>` does
    // `target.asType(<MT>)` off an `ldc` of a `CONSTANT_MethodType`, which the
    // interpreter used to refuse; `vm/src/runtime/interpreter/constants.rs`
    // decodes both tags today and does so mode-independently.
    //
    // MEASURED 2026-08-21 (`C:/craton/cratonvm-r5.exe`, oracle HotSpot
    // 25.0.3+9), the same program in three arms — the `--jdk-only` arm is the
    // experiment, because these rows are already dropped there as
    // `SyntheticStub`:
    //
    //   asInterfaceInstance(Greeter.class, mh).getClass()
    //     HotSpot     jdk.MHProxy1.…$Greeter/0x…       greet("bob") = "hi bob"
    //     --jdk-only  jdk.MHProxy1.…$Greeter           greet("bob") = "hi bob"
    //     Compatible  ClassCastException
    //   isWrapperInstance(<raw MethodHandle>)
    //     HotSpot false | --jdk-only false | Compatible TRUE
    //   wrapperInstanceTarget(<raw MethodHandle>)
    //     HotSpot IllegalArgumentException | --jdk-only IllegalArgumentException
    //     Compatible returns the handle
    //   two asInterfaceInstance calls yield distinct instances
    //     HotSpot true | --jdk-only true | Compatible FALSE
    //
    // WHY A GUARD AND NOT A DELETION. `docs/known-issues/jdk-only/H15-3` §1.4
    // proposed deleting these three outright. Deletion is not available to this
    // lane and would be red in CI: `vm/src/vm/tests.rs`'s
    // `method_handle_proxies_p68` calls `isWrapperInstance` through
    // `call_native`, which PANICS (`"{class}.{method}{descriptor} not
    // registered"`) on an absent registration, and
    // `cargo test -p cratonvm-vm --lib --features synthetic-jdk` is a blocking
    // job. That registry is built by `NativeMethodRegistry::new()` and never
    // calls `set_drop_real_layout_synthetic`, so the guard below leaves it —
    // and the synthetic-JDK image, which has no `MethodHandleProxies` bytecode
    // to fall back to — untouched.
    //
    // `drops_real_layout_synthetic()` is the predicate the registry documents
    // for exactly this question, and a `#[cfg(feature = "synthetic-jdk")]`
    // guard is explicitly NOT equivalent (`native-api/src/registry.rs`): the
    // Cargo feature decides what is compiled, the launcher flag decides which
    // class library loads. Both real-JDK arms of `vm_init` set the flag before
    // any `register_*` pass, so this drops in `--real-jdk`/Compatible AND in
    // `--jdk-only` — a no-op for the latter, which already dropped them.
    //
    // docs/known-issues/jdk-only/H19-1-three-stand-ins-retired-against-a-real-image-20260821.md §1
    if !r.drops_real_layout_synthetic() {
        r.register(
            mhp,
            "asInterfaceInstance",
            "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;",
            |_ctx, args| {
                // Return the method handle as the proxy (simplified)
                Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
            },
        );
    }
    // The three queries below used to be blanket `false` / `null` / `null`,
    // which directly contradicted what `asInterfaceInstance` had just handed
    // the caller: under this simplification a "wrapper instance" IS the
    // MethodHandle itself. Answer them consistently off that, so
    // `isWrapperInstance(asInterfaceInstance(...))` is true and the two
    // accessors return the target and its type instead of null.
    fn mhp_wrapper_handle(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> Result<Option<ObjectRef>, MethodCallFailed> {
        let Some(Value::Object(Some(obj))) = args.first().copied() else {
            return Ok(None);
        };
        let Some(mh_class_id) = ctx.class_id_by_name("java/lang/invoke/MethodHandle") else {
            return Ok(None);
        };
        let obj_class_id = ctx.class_id_of_object(obj);
        // An array's header carries its COMPONENT's class id: a
        // `MethodHandle[]` is not a wrapper instance.
        let is_array = matches!(ctx.heap_kind_of(obj), ObjectKind::Array);
        if !is_array && (obj_class_id == mh_class_id || ctx.is_subclass(obj_class_id, mh_class_id))
        {
            Ok(Some(obj))
        } else {
            Ok(None)
        }
    }
    // Same guard, same reasoning, and it has to be the same guard: these two
    // are only coherent BESIDE the `asInterfaceInstance` simplification above.
    // Dropping that one alone would leave `isWrapperInstance` answering `true`
    // for a raw handle that the real `asInterfaceInstance` never produced.
    if !r.drops_real_layout_synthetic() {
        r.register(
            mhp,
            "isWrapperInstance",
            "(Ljava/lang/Object;)Z",
            |ctx, args| {
                let present = mhp_wrapper_handle(ctx, args)?.is_some();
                Ok(Some(Value::Int(if present { 1 } else { 0 })))
            },
        );
        r.register(
            mhp,
            "wrapperInstanceTarget",
            "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            |ctx, args| match mhp_wrapper_handle(ctx, args)? {
                // Real JDK throws IllegalArgumentException for a non-wrapper; we
                // keep the historical null there so an existing caller that never
                // checked `isWrapperInstance` first does not start throwing.
                Some(mh) => Ok(Some(Value::Object(Some(mh)))),
                None => Ok(Some(Value::Object(None))),
            },
        );
    }
    // `wrapperInstanceType` is deliberately NOT registered. (Interpreter round
    // i4 wave 85, lane L4, correcting this note: JDK 25's
    // `MethodHandleProxies.wrapperInstanceType(Object)` returns the wrapped
    // INTERFACE, `(Ljava/lang/Object;)Ljava/lang/Class;`, read from the proxy
    // class's static `TYPE` field -- not a `MethodType`. Against a real image
    // the JDK body answers it; `L4W85MethodHandleProxies` pins that.) The
    // deleted constant-null registration was dead in synthetic mode as well:
    // `MethodHandleProxies` has no `synthetic_jdk_method_decls` entry and no
    // in-tree caller.

    // Round-9 perf: LambdaMetafactory CallSite cache. Each lambda shape
    // (functional_interface_type, samMethodType, instantiatedMethodType,
    // implMethod) is bootstrap-invariant — the same key always yields
    // a `CallSite` whose target MH is identical. Hot reflective dispatch
    // hits the same key thousands of times during request handling;
    // without this cache every hit re-allocates a MH + ConstantCallSite.
    //
    // GC: cached `ObjectRef` values are reported as roots via
    // `gc_scan_lambda_callsite_cache_roots` and remapped via
    // `gc_update_lambda_callsite_cache_refs` after compaction — same
    // contract as the Integer.valueOf cache in lang_math.rs.

    // LambdaMetafactory (static method stubs for bootstrap)
    //
    // These shims are hit when application code REFLECTIVELY invokes
    // `LambdaMetafactory.metafactory(...)` (the `invokedynamic` opcode itself
    // is short-circuited in `vm/src/runtime/invokedynamic.rs` and never calls
    // this path). Notable reflective callers include:
    //
    //   * log4j 2.x `ServiceLoaderUtil.loadClassloaderServices` — builds a
    //     `CallSite` via `metafactory(...)` then calls `cs.getTarget()` and
    //     `.invoke()` to materialise a `Stream<Provider>`.
    //   * Elasticsearch's CLI bootstrap goes through the same SPI helper.
    //
    // Returning `null` here makes the caller NPE inside `CallSite.getTarget()`
    // ("Cannot invoke getTarget on null"). Worse, the NPE is rethrown out of
    // the SPI loop so the ServiceLoader silently produces zero providers —
    // which is what makes Elasticsearch report
    // `CliToolProvider [server] not found, available names are []`.
    //
    // We can't faithfully reproduce the metafactory's lambda-proxy synthesis
    // here (the bytecode-level path needs the BSM static-args, not the
    // reflective arg list). What we CAN do is hand back a non-null
    // ConstantCallSite whose target is a no-op MethodHandle. When the caller
    // subsequently invokes the SAM, the no-op MH dispatches via
    // `mh_dispatch`, which returns null for an unknown target — exactly what
    // log4j/ES interpret as "no service provider", and crucially avoids the
    // upstream NPE.
    // metafactory / altMetafactory are a real bridge (see header), not stubs.
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let lmf = "java/lang/invoke/LambdaMetafactory";
    r.register(lmf, "metafactory",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;",
        |ctx, args| {
            if ctx.lambda_metafactory_reflective_to_jdk() {
                return lmf_reflective_to_jdk(ctx, "metafactory", LMF_METAFACTORY_DESC, args);
            }
            // The JDK's checks first (wave 50), on a cache miss only: an
            // entry is a call that linked, and so passed them. `args` is
            // re-read after them.
            let cached_before = {
                let key = LambdaKey {
                    host: reflective_lambda_host(&*ctx, args.first()).map_or(0, |h| h.as_u32()),
                    invoked: lambda_key_of(match args.get(2) { Some(Value::Object(o)) => *o, _ => None }),
                    sam: lambda_key_of(match args.get(3) { Some(Value::Object(o)) => *o, _ => None }),
                    impl_: lambda_key_of(match args.get(4) { Some(Value::Object(o)) => *o, _ => None }),
                    instantiated: lambda_key_of(match args.get(5) { Some(Value::Object(o)) => *o, _ => None }),
                };
                let c = lambda_callsite_cache().lock();
                if key.impl_ != 0 { c.get(&(ctx.vm_identity(), key)).copied() } else { None }
            };
            if let Some(cached) = cached_before {
                return Ok(Some(Value::Object(Some(cached))));
            }
            let checked = lmf_bridge_jdk_checks(ctx, args, false)?;
            let args = checked.as_slice();
            // Reflective `LambdaMetafactory.metafactory`. (The `invokedynamic`
            // opcode is handled inline in `runtime/invokedynamic.rs` and never
            // reaches this native — only reflective callers like log4j2's
            // `ServiceLoaderUtil.loadClassloaderServices` / the Elasticsearch
            // CLI bootstrap do.)  args:
            //   [0]=Lookup, [1]=String invokedName,
            //   [2]=MethodType invokedType (factory signature),
            //   [3]=MethodType samMethodType, [4]=MethodHandle implMethod,
            //   [5]=MethodType instantiatedMethodType.
            let invoked_type = match args.get(2) { Some(Value::Object(o)) => *o, _ => None };
            let sam_type     = match args.get(3) { Some(Value::Object(o)) => *o, _ => None };
            let impl_method  = match args.get(4) { Some(Value::Object(o)) => *o, _ => None };
            let inst_type    = match args.get(5) { Some(Value::Object(o)) => *o, _ => None };
            // The proxy's host (HotSpot: `caller.lookupClass()`), read before
            // anything can allocate.
            let host = reflective_lambda_host(&*ctx, args.first());
            let host_raw = host.map_or(0, |h| h.as_u32());

            // Bootstrap-arg cache: identical (lookup class, invokedType,
            // samType, implMethod, instantiatedType) tuples always yield the
            // same CallSite.
            let key = LambdaKey {
                host: host_raw,
                invoked: lambda_key_of(invoked_type),
                sam: lambda_key_of(sam_type),
                impl_: lambda_key_of(impl_method),
                instantiated: lambda_key_of(inst_type),
            };
            if key.impl_ != 0 {
                if let Some(cached) = { let c = lambda_callsite_cache().lock(); c.get(&(ctx.vm_identity(), key)).copied() } {
                    return Ok(Some(Value::Object(Some(cached))));
                }
            }

            let invoked_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };

            // gen r4w3/rooting: the fallback below re-reads args[2] after
            // `build_reflective_lambda_callsite` / `alloc_method_handle` (GC
            // points), and a slice re-read is stale; pin a copy now.
            // gen r4w4/rooting2: all four key objects are pinned, so the
            // cache entry is keyed by their POST-build addresses (see
            // `lambda_key_from_pins`).
            let key_objs = [invoked_type, sam_type, impl_method, inst_type];
            let key_pins = key_objs.map(|o| o.map(|r| ctx.pin_native_root(r)));
            let key_pin_base = key_pins.iter().flatten().next().copied();
            let invoked_type_pin = key_pins[0];
            // Preferred path: synthesise a genuine lambda proxy + factory MH so
            // `cs.getTarget().bindTo(..).invoke()` yields a working SAM instance
            // whose abstract method runs the impl method.
            if let Ok(Some(ccs)) = build_reflective_lambda_callsite(
                ctx, host, invoked_type, &invoked_name, sam_type, impl_method, inst_type, false,
                &[], &[],
            ) {
                let insert_key = lambda_key_from_pins(&*ctx, host_raw, key_objs, key_pins);
                if let Some(b) = key_pin_base {
                    ctx.unpin_native_roots(b);
                }
                if key.impl_ != 0 {
                    lambda_callsite_cache().lock().insert((ctx.vm_identity(), insert_key), ccs);
                }
                return Ok(Some(Value::Object(Some(ccs))));
            }

            // Fallback (implMethod missing / proxy registration unavailable): a
            // FROZEN ConstantCallSite wrapping a no-op MH. Still non-null and
            // getTarget()-safe (the real ConstantCallSite.getTarget() throws
            // IllegalStateException unless isFrozen).
            tracing::warn!(
                "LambdaMetafactory.metafactory: could not synthesise lambda proxy \
                 (invokedName='{}') — returning frozen ConstantCallSite with no-op MH",
                invoked_name
            );
            let noop = alloc_method_handle(
                ctx,
                "java/lang/invoke/LambdaMetafactory$NoOp",
                if invoked_name.is_empty() { "apply" } else { invoked_name.as_str() },
                "()Ljava/lang/Object;",
                MH_KIND_STATIC,
            )?;
            // gen r4w3/rooting: the pinned copy of args[2], re-read.
            if let (Some(mt), Some(p)) = (invoked_type, invoked_type_pin) {
                let mt = ctx.read_native_pin(p, mt);
                ctx.set_field_by_name(noop, "type", Value::Object(Some(mt)));
            }
            if let Some(b) = key_pin_base {
                ctx.unpin_native_roots(b);
            }
            Ok(Some(Value::Object(Some(alloc_frozen_constant_call_site(ctx, noop)?))))
        });
    r.register(lmf, "altMetafactory",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
        |ctx, args| {
            if ctx.lambda_metafactory_reflective_to_jdk() {
                return lmf_reflective_to_jdk(ctx, "altMetafactory", LMF_ALT_METAFACTORY_DESC, args);
            }
            // The JDK's checks first (wave 50); `args` is re-read after them.
            let checked = lmf_bridge_jdk_checks(ctx, args, true)?;
            let args = checked.as_slice();
            // `altMetafactory` packs (samMethodType, implMethod,
            // instantiatedMethodType, flags, markerInterfaces, ...) into
            // args[3]: Object[]. invokedType is args[2] (factory signature).
            // The host is the lookup class, as in `metafactory` above.
            let host = reflective_lambda_host(&*ctx, args.first());
            let host_raw = host.map_or(0, |h| h.as_u32());
            let invoked_type = match args.get(2) { Some(Value::Object(o)) => *o, _ => None };
            let (sam_type, impl_method, inst_type, flags, marker_names, bridge_descs) = match args.get(3) {
                Some(Value::Object(Some(arr))) => {
                    let arr = *arr;
                    let len = ctx.array_length(arr);
                    let e = |ctx: &dyn NativeContext, i: usize| if i < len {
                        match ctx.get_array_element(arr, i) { Value::Object(o) => o, _ => None }
                    } else { None };
                    // args[3] of the packed array is the flags bitmask (a boxed
                    // Integer); FLAG_SERIALIZABLE is 0x1. Without it the spun proxy
                    // would claim a `writeReplace()` and `Serializable` that the real
                    // JDK only grants to Serializable-intersected call sites.
                    let flags = if 3 < len {
                        match ctx.get_array_element(arr, 3) {
                            Value::Int(i) => i,
                            Value::Object(Some(b)) => match ctx.get_field_by_name(b, "value") {
                                Value::Int(i) => i,
                                _ => 0,
                            },
                            _ => 0,
                        }
                    } else {
                        0
                    };
                    // FLAG_MARKERS (0x2): arr[4] is markerCount, then that many
                    // `Class` objects — interfaces the spun proxy must implement
                    // ON TOP of the SAM. Same layout the `invokedynamic`
                    // bootstrap reads out of the BSM static args (see
                    // `vm/src/runtime/invokedynamic.rs::read_marker_interfaces`).
                    let count_at = |ctx: &dyn NativeContext, i: usize| -> usize {
                        if i < len {
                            match ctx.get_array_element(arr, i) {
                                Value::Int(n) => n.max(0) as usize,
                                Value::Object(Some(b)) => match ctx.get_field_by_name(b, "value") {
                                    Value::Int(n) => n.max(0) as usize,
                                    _ => 0,
                                },
                                _ => 0,
                            }
                        } else {
                            0
                        }
                    };
                    let mut markers: Vec<String> = Vec::new();
                    // Where the FLAG_BRIDGES block starts: right after the
                    // markers when FLAG_MARKERS is set, else at arr[4].
                    let mut bridge_at = 4usize;
                    if (flags & 0x2) != 0 {
                        let count = count_at(ctx, 4);
                        for k in 0..count.min(len) {
                            if let Some(m) = e(ctx, 5 + k) {
                                if let Some(name) = mirror_class_name(ctx, m) {
                                    markers.push(name);
                                }
                            }
                        }
                        bridge_at = 5usize.saturating_add(count);
                    }
                    // FLAG_BRIDGES (0x4): bridgeCount, then that many
                    // `MethodType`s — extra descriptors of the SAM name the
                    // spun class implements (`read_bridge_descriptors` is the
                    // `invokedynamic` twin).
                    let mut bridges: Vec<String> = Vec::new();
                    if (flags & 0x4) != 0 {
                        let count = count_at(ctx, bridge_at);
                        for k in 0..count.min(len) {
                            if let Some(mt) = e(ctx, bridge_at.saturating_add(1 + k)) {
                                bridges.push(descriptor_from_method_type(ctx, mt));
                            }
                        }
                    }
                    (e(ctx, 0), e(ctx, 1), e(ctx, 2), flags, markers, bridges)
                }
                _ => (None, None, None, 0, Vec::new(), Vec::new()),
            };
            let ser_flag = (flags & 0x1) != 0;
            let key = LambdaKey {
                host: host_raw,
                invoked: lambda_key_of(invoked_type),
                sam: lambda_key_of(sam_type),
                impl_: lambda_key_of(impl_method),
                instantiated: lambda_key_of(inst_type),
            };
            // The identity cache keys ONLY on the four bootstrap-arg objects.
            // The JDK interns `MethodType`s, so
            //   metafactory(lookup, "add", ()LAdder;, (II)I, impl, (II)I)
            // and
            //   altMetafactory(lookup, "add", ()LAdder;,
            //                  {(II)I, impl, (II)I, FLAG_MARKERS, 1, Cloneable})
            // produce the SAME key while needing DIFFERENT proxy interface
            // lists. A flags-bearing call site must therefore neither read nor
            // populate the cache. Flags are rare on this reflective path.
            let cacheable = flags == 0;
            if cacheable && key.impl_ != 0 {
                if let Some(cached) = { let c = lambda_callsite_cache().lock(); c.get(&(ctx.vm_identity(), key)).copied() } {
                    return Ok(Some(Value::Object(Some(cached))));
                }
            }
            let invoked_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // gen r4w3/rooting: the fallback below re-reads args[2] after
            // `build_reflective_lambda_callsite` / `alloc_method_handle` (GC
            // points), and a slice re-read is stale; pin a copy now.
            // gen r4w4/rooting2: all four key objects pinned, as in
            // `metafactory` above.
            let key_objs = [invoked_type, sam_type, impl_method, inst_type];
            let key_pins = key_objs.map(|o| o.map(|r| ctx.pin_native_root(r)));
            let key_pin_base = key_pins.iter().flatten().next().copied();
            let invoked_type_pin = key_pins[0];
            if let Ok(Some(ccs)) = build_reflective_lambda_callsite(
                ctx, host, invoked_type, &invoked_name, sam_type, impl_method, inst_type,
                ser_flag, &marker_names, &bridge_descs,
            ) {
                let insert_key = lambda_key_from_pins(&*ctx, host_raw, key_objs, key_pins);
                if let Some(b) = key_pin_base {
                    ctx.unpin_native_roots(b);
                }
                if cacheable && key.impl_ != 0 {
                    lambda_callsite_cache().lock().insert((ctx.vm_identity(), insert_key), ccs);
                }
                return Ok(Some(Value::Object(Some(ccs))));
            }
            tracing::warn!(
                "LambdaMetafactory.altMetafactory: could not synthesise lambda proxy \
                 (invokedName='{}') — returning frozen ConstantCallSite with no-op MH",
                invoked_name
            );
            let noop = alloc_method_handle(
                ctx,
                "java/lang/invoke/LambdaMetafactory$NoOp",
                if invoked_name.is_empty() { "apply" } else { invoked_name.as_str() },
                "()Ljava/lang/Object;",
                MH_KIND_STATIC,
            )?;
            // gen r4w3/rooting: the pinned copy of args[2], re-read.
            if let (Some(mt), Some(p)) = (invoked_type, invoked_type_pin) {
                let mt = ctx.read_native_pin(p, mt);
                ctx.set_field_by_name(noop, "type", Value::Object(Some(mt)));
            }
            if let Some(b) = key_pin_base {
                ctx.unpin_native_roots(b);
            }
            Ok(Some(Value::Object(Some(alloc_frozen_constant_call_site(ctx, noop)?))))
        });

    // NB: an earlier draft added a short-circuit for
    // `org/apache/logging/log4j/util/ServiceLoaderUtil.loadClassloaderServices`
    // that returned an empty Stream. The metafactory fix above is enough
    // to suppress the NPE chain by itself, and short-circuiting the SPI
    // would also mask any real provider discovery that succeeds via the
    // normal path. If the metafactory fix proves insufficient, re-add the
    // short-circuit at this point (returning a synthetic empty Stream
    // object with field 0 = null array, field 1 = Int(0)).

    // StringConcatFactory — already registered in Phase 58 with full implementation
    r.set_category(__prev_cat);
}

#[cfg(test)]
mod j27_wmte1_unbox_raise_chain_tests {
    //! JIT round j27 wave 4, lane MHROWS2 (WMTE-1): the JDK frames HotSpot
    //! lists above an `asType` unbox refusal the VM serves natively.
    use super::*;
    use cratonvm_native_api::ServedRaiseHopSite as Site;

    fn shape(chain: &cratonvm_native_api::ServedRaiseChain) -> Vec<(&str, &str, Site)> {
        chain.hops.iter().map(|h| (h.class, h.name, h.site)).collect()
    }

    /// `asType(Object -> int)` of a `Long`: `unboxInteger(Object, false)` ->
    /// `primitiveConversion` -> `Integer.class.cast(x)` raises
    /// `Cannot cast java.lang.Long to java.lang.Integer` in `Class.cast`
    /// (HotSpot 25: `unboxInteger:81, primitiveConversion:252, Class.cast`).
    #[test]
    fn unconvertible_unbox_lists_value_conversions_and_class_cast() {
        let chain = unbox_raise_chain("I", UnboxRaise::Unconvertible).expect("int unboxes");
        assert_eq!(chain.throwable_class, "java/lang/ClassCastException");
        assert_eq!(
            shape(&chain),
            vec![
                ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
                (
                    "sun/invoke/util/ValueConversions",
                    "unboxInteger",
                    Site::LastCall {
                        name: "primitiveConversion",
                        descriptor: Some(
                            "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;"
                        ),
                    },
                ),
                (
                    "sun/invoke/util/ValueConversions",
                    "primitiveConversion",
                    Site::LastCall {
                        name: "cast",
                        descriptor: Some("(Ljava/lang/Object;)Ljava/lang/Object;"),
                    },
                ),
                ("java/lang/Class", "cast", Site::Raise),
            ]
        );
        assert_eq!(chain.hops[1].descriptor, "(Ljava/lang/Object;Z)I");
    }

    /// `asType(Object -> int)` of a `String`: `primitiveConversion`'s
    /// `(Number) x` raises (HotSpot 25: `unboxInteger:81,
    /// primitiveConversion:247`), a `checkcast`, not a construction.
    #[test]
    fn not_a_number_unbox_raises_at_the_number_checkcast() {
        let chain = unbox_raise_chain("Z", UnboxRaise::NotANumber).expect("boolean unboxes");
        assert_eq!(chain.throwable_class, "java/lang/ClassCastException");
        assert_eq!(
            shape(&chain),
            vec![
                ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
                (
                    "sun/invoke/util/ValueConversions",
                    "unboxBoolean",
                    Site::LastCall {
                        name: "primitiveConversion",
                        descriptor: Some(
                            "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;"
                        ),
                    },
                ),
                (
                    "sun/invoke/util/ValueConversions",
                    "primitiveConversion",
                    Site::LocalCheckcast {
                        class_name: "java/lang/Number"
                    },
                ),
            ]
        );
    }

    /// The null chain keeps its pre-j27 shape (wave 60, lane L3).
    #[test]
    fn null_unbox_raises_in_the_unbox_method() {
        let chain = unbox_raise_chain("J", UnboxRaise::NullReturn).expect("long unboxes");
        assert_eq!(chain.throwable_class, "java/lang/NullPointerException");
        assert_eq!(
            shape(&chain),
            vec![
                ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
                ("sun/invoke/util/ValueConversions", "unboxLong", Site::Raise),
            ]
        );
        assert_eq!(chain.hops[1].descriptor, "(Ljava/lang/Object;Z)J");
    }

    /// A real field handle's adapted receiver cast raises in `Class.cast`.
    #[test]
    fn reference_cast_lists_class_cast_above_the_invoker() {
        let chain = reference_cast_raise_chain();
        assert_eq!(chain.throwable_class, "java/lang/ClassCastException");
        assert_eq!(
            shape(&chain),
            vec![
                ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
                ("java/lang/Class", "cast", Site::Raise),
            ]
        );
    }

    #[test]
    fn a_reference_descriptor_has_no_unbox_chain() {
        assert!(unbox_raise_chain("Ljava/lang/Object;", UnboxRaise::Unconvertible).is_none());
        assert!(unbox_raise_chain("V", UnboxRaise::NullReturn).is_none());
    }

    /// MH16-10 (j27 wave 4, lane MHROWS2): the moving mock remaps the per-VM
    /// `valueOf` caches as the VM's GC does (`gc_update_value_of_cache_refs`),
    /// so a cached box asked for again after a move is the MOVED box, as on
    /// the VM, and a moving test may box inside `-128..127`.
    #[test]
    fn the_moving_mock_forwards_the_value_of_cache() {
        #[allow(unused_imports)]
        use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
        let mut ctx = crate::test_utils::MockNativeContext::new();
        ctx.set_vm_identity(0x2710_4D16);
        let _ = ctx.ensure_class_initialized("java/lang/Integer");
        ctx.set_moving_heap_vm_rule(true);
        let boxed = |ctx: &mut crate::test_utils::MockNativeContext| {
            match crate::lang_math::native_integer_value_of(ctx, &[Value::Int(7)]) {
                Ok(Some(Value::Object(Some(o)))) => o,
                other => panic!("Integer.valueOf(7): {other:?}"),
            }
        };
        let first = boxed(&mut ctx);
        let pin = ctx.pin_native_root(first);
        // A GC point in both moving modes (its answer is the scripted help).
        let _ = ctx.reclaim_before_alloc_retry();
        let moved = ctx.read_native_pin(pin, first);
        assert_ne!(moved, first, "the reclaim must move the heap");
        assert_eq!(boxed(&mut ctx), moved, "valueOf(7) answers the moved canonical box");
    }

    /// The refusal the caller sees is unchanged: `Class.cast`'s wording.
    #[test]
    fn unconvertible_wrapper_still_refused_with_class_cast_wording() {
        #[allow(unused_imports)]
        use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let _ = ctx.ensure_class_initialized("java/lang/Long");
        let boxed = match ctx.new_object("java/lang/Long") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object Long: {other:?}"),
        };
        ctx.set_field(boxed, 0, Value::Long(6));
        let refused = mh_unbox_for_primitive(&mut ctx, "I", Value::Object(Some(boxed)), None);
        match refused {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message, .. },
            ))) => assert_eq!(message, "Cannot cast java.lang.Long to java.lang.Integer"),
            other => panic!("not a ClassCastException: {other:?}"),
        }
    }
}

