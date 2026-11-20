#[allow(unused_imports)]
use super::*;

/// The cold half of [`mh_constant_bind_redirect`]: ask the admission about the
/// handle at `raw` and (re)bind the site. Holds the JIT thread only for the
/// admission's reads.
///
/// Round 16 wave 15 (lane mh6; MHB-1): when the exact admission refuses and
/// `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING` is on, the converting admission
/// (`lang_invoke::mh_compiled_bind_converting_target`) is asked next, and its
/// site records the [`MhBindConv`] the warm path applies.
#[cold]
pub unsafe fn mh_constant_bind_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    site_key: usize,
    raw: u64,
    vm_id: usize,
    epoch: u64,
) -> Option<MhBindHit> {
    // A re-fill within the epoch of the last one is churn (another handle at
    // this site); one after a collection is the same site re-validating its
    // moved handle, which must not count towards giving up.
    // gcj j3/rt3: only this site's own row carries its budget forward; a row
    // left by another site at a re-issued info address starts it again.
    let identity = MhSiteIdentity::of(info);
    let retire_epoch = identity.retire_epoch.get();
    // JIT round 21 wave 19 (lane mhbind2): and the previous real-bound shape,
    // of this VM, for `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES`.
    let (prev_fills, prev_epoch, prev_real_shape) = MH_BIND_SITES.with(|sites| {
        let sites = sites.borrow();
        sites
            .get(&site_key)
            .filter(|s| s.identity.names(info, retire_epoch, mh_site_identity_enabled))
            .map_or((0, u64::MAX, None), |s| {
                (s.fills, s.epoch, s.real_shape.filter(|_| s.vm == vm_id))
            })
    });
    if prev_fills > MH_BIND_MAX_FILLS {
        return None;
    }
    let fills = if prev_epoch == epoch {
        prev_fills + 1
    } else {
        prev_fills
    };
    let obj = vm.mem.heap.is_object_address(raw as usize)?;
    // MH7-3 (JIT round 17 wave 2, lane mh2;
    // `CRATONVM_JIT_MH_CONSTANT_IMMUTABLE_CHECK`, default ON): a handle the
    // compile doors may call as its member must not have been rewritten in
    // place since its admission. Read here, on the cold fill.
    use crate::runtime::interpreter::jit_bridge as mh_memo;
    let compile_memo = mh_memo::mh_constant_compile_enabled(vm);
    let check_words = compile_memo && mh_memo::mh_constant_immutable_check_enabled();
    if check_words {
        mh_memo::mh_constant_guard_verify(vm, raw, epoch, || mh_constant_handle_words(vm, obj));
        if mh_memo::mh_constant_guard_sweep_dbg() {
            mh_memo::mh_constant_guard_sweep(vm, epoch, &|h: u64| {
                mh_constant_handle_words(vm, ObjectRef::from_raw(h as *mut u8))
            });
        }
    }
    let (target, conversion, real, real_receiver, real_conversion) = {
        let (thread, _guard) = jit_thread_mut()?;
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let real = cratonvm_native_builtins::lang_invoke::mh_is_real_handle(&ctx, obj);
        let bind = match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_target(
            &mut ctx,
            obj,
            info.descriptor,
        ) {
            Some(target) => (Some(target), None, real),
            // MHB-1 (round 16 wave 15, lane mh6): read here, on the cold fill.
            None if !real && mh_constant_bind_converting_flag() => {
                match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_converting_target(
                    &mut ctx,
                    obj,
                    info.descriptor,
                    info.method_name == "invoke",
                ) {
                    Some((target, conversion)) => (Some(target), Some(conversion), real),
                    None => (None, None, real),
                }
            }
            None => (None, None, real),
        };
        // THROWS18-4 (round 18 wave 11, lane mhresid): an interface member,
        // behind the per-call receiver type guard. Read here, on the cold fill.
        let bind = if bind.0.is_none()
            && cratonvm_native_builtins::lang_invoke::mh_constant_bind_interface_enabled()
        {
            match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_interface_target(
                &mut ctx,
                obj,
                info.descriptor,
            ) {
                Some(target) => (Some(target), None, real),
                None => bind,
            }
        } else {
            bind
        };
        // Interpreter round i5 wave 109, lane L2
        // (`CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL`, default OFF): a real
        // `$Special` handle, or a plain one over a private method, as an
        // `invoke_kind` 1 (`invokespecial`) call; its null receiver is screened
        // per call by the redirect. Read here, on the cold fill.
        let bind = if bind.0.is_none()
            && real
            && cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_special_enabled()
        {
            match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_special_target(
                &mut ctx,
                obj,
                info.descriptor,
            ) {
                Some(target) => (Some(target), None, real),
                None => bind,
            }
        } else {
            bind
        };
        // j27 wave 5 (lane MHASTYPE;
        // `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`, default ON): a REAL
        // `asType` result every arm above refused. Read here, on the cold fill.
        let real_converting = if bind.0.is_none()
            && real
            && cratonvm_native_builtins::lang_invoke::mh_constant_bind_real_converting_enabled()
        {
            cratonvm_native_builtins::lang_invoke::mh_real_converting_compiled_bind(
                &mut ctx,
                obj,
                info.descriptor,
            )
        } else {
            None
        };
        let (bind, real_conversion) = match real_converting {
            Some((target, c)) => ((Some(target), None, real), Some(c)),
            None => (bind, None),
        };
        // MH6-3 (JIT round 21 wave 17, lane mhbind): a REAL `bindTo` result's
        // bound value, the call's receiver (its `argL1`, a `final` field).
        let real_receiver = match &bind.0 {
            Some(t) if real && t.bound => {
                cratonvm_native_builtins::lang_invoke::mh_real_bound_receiver(&ctx, obj)
                    .map(|r| r.as_ptr() as u64)
            }
            _ => None,
        };
        (bind.0, bind.1, bind.2, real_receiver, real_conversion)
    };
    let conv = conversion.and_then(|c| mh_bind_conv_of(vm, obj, &c));
    // j27 wave 5 (lane MHASTYPE): a real `asType` result's wrapper slots.
    let real_conv = real_conversion.and_then(|c| mh_real_conv_of(vm, &c));
    // A conversion whose slots do not resolve keeps the door.
    let target = if (conversion.is_some() && conv.is_none())
        || (real_conversion.is_some() && real_conv.is_none())
    {
        None
    } else {
        target
    };
    // JIT round 17 wave 2 (lane mh2;
    // `r17-mh-constant-bind-admission-is-loader-blind-FIXED-20261005.md`): the
    // admission compared descriptor STRINGS. The compiled caller's loader
    // (`declaring_class_id`, the class the site lives in) and the member's
    // must see every class the site names as one, the compile door's rule
    // (`CRATONVM_JIT_MH_CONSTANT_LOADER_CHECK`); otherwise the door, which
    // judges the call against the caller's classes, keeps the site.
    let target = target.filter(|t| {
        crate::runtime::interpreter::jit_bridge::mh_constant_bind_loaders_agree(
            vm,
            info.declaring_class_id,
            t.class_id,
            info.descriptor,
        )
    });
    let leaf = target.as_ref().and_then(|t| mh_bind_leaf_info(vm_id, t));
    let bound = target.as_ref().is_some_and(|t| t.bound);
    // THROWS18-4: the interface whose implementors the warm path admits.
    let iface = match (&target, leaf) {
        (Some(t), Some(_)) if t.invoke_kind == 2 => t.class_id,
        _ => 0,
    };
    // MH16-8 (round 16 wave 10, lane mhfloor): read here, on the cold fill.
    // JIT round 19 wave 1 (lane mhiface): an interface site only with
    // `CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT_INTERFACE`.
    let direct = leaf.is_some()
        && mh_constant_bind_direct_flag()
        && (iface == 0 || mh_constant_bind_direct_interface_flag());
    let mut guard = [0u64; 7];
    guard[0] = raw;
    // A real handle has none of these slots (the redirect's warm path takes its
    // address and epoch as the whole guard, `MhBindHit::real`).
    if leaf.is_some() && !real {
        let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
        for (i, slot) in slots.iter().enumerate() {
            guard[i + 1] = mh_bind_slot_key(vm.mem.heap.get_field(obj, *slot));
        }
    }
    // MH6-3 (JIT round 21 wave 17, lane mhbind): a REAL `bindTo` result's
    // bound value stands where a carrier's `MH_BOUND` guard word does, which
    // is where the redirect reads a bound call's receiver. Its `argL1` is
    // `final`, so within this epoch the handle's address answers it. No value
    // leaves the word `0`, which the redirect hands to the door.
    let mut real_shape = None;
    if leaf.is_some() && real && bound {
        if let Some(receiver) = real_receiver {
            guard[1 + cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_BOUND_GUARD] =
                receiver;
            // MHB-3: read here, on the cold fill.
            {
                real_shape = mh_real_bound_shape_of(vm, obj, receiver);
                // JIT round 21 wave 19 (lane mhbind2): a second bound-value
                // class of the same shape, same epoch.
                if let (Some(now), Some(prev)) = (real_shape.as_mut(), prev_real_shape) {
                    let same_but_class = MhRealBoundShape {
                        receiver_class: now.receiver_class,
                        receiver_class_alt: 0,
                        ..prev
                    } == *now;
                    if prev_epoch == epoch
                        && same_but_class
                        && prev.receiver_class != now.receiver_class
                    {
                        now.receiver_class_alt = prev.receiver_class;
                    }
                }
            }
        }
    }
    // MHF-1 (round 16 wave 11, lane mhbind): read here, on the cold fill. A
    // converting site records no shape (its guard is address-keyed only).
    let shape = if leaf.is_some() && !real && conv.is_none() && mh_constant_bind_shape_flag() {
        mh_bind_shape_of(vm, obj)
    } else {
        None
    };
    let conv = if leaf.is_some() { conv } else { None };
    let real_conv = if leaf.is_some() { real_conv } else { None };
    // MH16-8 at compile time (round 16 wave 17, lane mh7): an exact direct
    // STATIC handle is recorded for the compile doors, which call it as its
    // member when a `static final` field holds it
    // (`jit_bridge::CpResolvers::mh_constant`). Read here, on the cold fill.
    // JIT round 17 wave 3 (lane mh4; MHCONST17-2): an unbound VIRTUAL handle
    // too, under `CRATONVM_JIT_MH_CONSTANT_VIRTUAL` (`mh_constant_records_kind`).
    if let Some(t) = target.as_ref().filter(|t| {
        leaf.is_some()
            && conv.is_none()
            // j27 wave 5 (lane MHASTYPE): a converting real handle is no
            // compile row (the compile door would call the member unconverted).
            && real_conv.is_none()
            && !t.bound
            && crate::runtime::interpreter::jit_bridge::mh_constant_records_kind(t.invoke_kind)
    }) {
        if compile_memo {
            // MH7-3: the words the compile-time bind relies on, as admitted.
            if check_words {
                if let Some(words) = mh_constant_handle_words(vm, obj) {
                    crate::runtime::interpreter::jit_bridge::note_mh_constant_guard(
                        vm, raw, epoch, words,
                    );
                }
            }
            crate::runtime::interpreter::jit_bridge::note_mh_constant_target_kind(
                vm,
                raw,
                epoch,
                info.descriptor,
                (&t.class, &t.name, &t.desc),
                t.class_id,
                t.invoke_kind,
            );
        }
    } else if let (Some(t), Some(rc)) = (target.as_ref(), real_conv.as_ref()) {
        if compile_memo
            && leaf.is_some()
            && conv.is_none()
            && real
            && !t.bound
            && matches!(t.invoke_kind, 0 | 2 | 3)
            && crate::runtime::interpreter::jit_bridge::mh_constant_compile_converting_enabled()
        {
            if let Some(row) = mh_real_convert_compile_row(vm, t.invoke_kind, info.descriptor, rc)
            {
                if check_words {
                    if let Some(words) = mh_constant_handle_words(vm, obj) {
                        crate::runtime::interpreter::jit_bridge::note_mh_constant_guard(
                            vm, raw, epoch, words,
                        );
                    }
                }
                crate::runtime::interpreter::jit_bridge::note_mh_constant_target_converting(
                    vm,
                    raw,
                    epoch,
                    info.descriptor,
                    (&t.class, &t.name, &t.desc),
                    t.class_id,
                    row,
                );
            }
        }
    }
    // MHBIND21-1 (JIT round 21 wave 19, lane mhbind2;
    // `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND`): a REAL `bindTo` result of a
    // virtual member, bound above with its `argL1` as the receiver, is
    // recorded for the compile doors with that field's slot and the bound
    // value's exact class, so a `static final` field holding it compiles as a
    // plain `invokevirtual` on the bound value (`invoke_kind 4`). Read here,
    // on the cold fill.
    if let (Some(t), Some(receiver)) = (target.as_ref(), real_receiver) {
        if compile_memo
            && leaf.is_some()
            && conv.is_none()
            && real
            && t.bound
            && t.invoke_kind == 0
            && crate::runtime::interpreter::jit_bridge::mh_constant_compile_bound_enabled()
        {
            if let Some((slot, class)) = mh_real_bound_compile_row(vm, obj, receiver) {
                if mh_constant_bind_dbg() {
                    eprintln!(
                        "[mh-bind] compile row {} -> {}.{}{} bound, argL1 slot {slot}, class {class}",
                        info.descriptor,
                        t.class,
                        t.name,
                        t.desc,
                    );
                }
                crate::runtime::interpreter::jit_bridge::note_mh_constant_target_bound(
                    vm,
                    raw,
                    epoch,
                    info.descriptor,
                    (&t.class, &t.name, &t.desc),
                    t.class_id,
                    slot,
                    class,
                );
            }
        }
    }
    // j27 wave 6 (lane T1LAM2; `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`,
    // default ON since 2026-10-28, j27 WARNING flip; =0 restores): a REAL `bindTo` result of a STATIC member (MH16-7's bind, its
    // bound value the member's leading argument) is recorded for the compile
    // doors with its `argL1` slot, so a `static final` field holding it
    // compiles as `invokestatic member(argL1, a*)` (`invoke_kind 6`). Never
    // as a plain kind 3 (the `!t.bound` gate above): that call would drop
    // the bound argument. Read here, on the cold fill.
    if let (Some(t), Some(value)) = (target.as_ref(), real_receiver) {
        if mh_bound_static_compile_row_wanted(
            compile_memo && leaf.is_some() && real,
            conv.is_none() && real_conv.is_none(),
            t.bound,
            t.invoke_kind,
        ) && crate::runtime::interpreter::jit_bridge::mh_constant_compile_bound_static_enabled()
        {
            if let Some((slot, _)) = mh_real_bound_compile_row(vm, obj, value) {
                if mh_constant_bind_dbg() {
                    eprintln!(
                        "[mh-bind] compile row {} -> {}.{}{} bound-static, argL1 slot {slot}",
                        info.descriptor, t.class, t.name, t.desc,
                    );
                }
                crate::runtime::interpreter::jit_bridge::note_mh_constant_target_bound_static(
                    vm,
                    raw,
                    epoch,
                    info.descriptor,
                    (&t.class, &t.name, &t.desc),
                    t.class_id,
                    slot,
                );
            }
        }
    }
    // MHBIND2-1 (JIT round 21 wave 20, lane mhfilter;
    // `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER`): a REAL filter species every
    // bind refused is recorded for the compile doors, which compile a `static
    // final` field holding it as two plain calls (`invoke_kind 5`). Read here,
    // on the cold fill.
    if target.is_none()
        && real
        && compile_memo
        && crate::runtime::interpreter::jit_bridge::mh_constant_compile_filter_enabled()
    {
        mh_constant_filter_compile_fill(vm, info, obj, raw, epoch);
    }
    // mh21 (the census step of MHBIND21-3 / MHFILTER21-3 / MHFILTER21-4):
    // under `CRATONVM_DBG_JIT_MH_BIND=1` a REAL species no bind admitted is
    // named with its species class and transform key.
    if target.is_none() && real && mh_constant_bind_dbg() {
        mh_constant_real_species_census(vm, info, obj);
    }
    if mh_constant_bind_dbg() {
        mh_constant_bind_dbg_line(info, leaf, bound, conv.as_ref());
        if let (Some(l), Some(c)) = (leaf, real_conv.as_ref()) {
            // j27 wave 5 (lane MHASTYPE): the engagement record of
            // `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`.
            eprintln!(
                "[mh-bind] MethodHandle.{}{} -> {}.{}{} real-converting unbox={:?} box={:?}",
                info.method_name,
                info.descriptor,
                l.class_name,
                l.method_name,
                l.descriptor,
                &c.unbox[..l.num_jit_args.min(MH_BIND_MAX_SPLICED_ARGS)],
                c.box_return
            );
        }
    }
    MH_BIND_SITES.with(|sites| {
        let mut sites = sites.borrow_mut();
        if sites.len() < MH_BIND_MAX_SITES || sites.contains_key(&site_key) {
            sites.insert(
                site_key,
                MhBindSite {
                    vm: vm_id,
                    epoch,
                    guard,
                    leaf,
                    bound,
                    direct,
                    shape,
                    conv,
                    fills,
                    real,
                    iface,
                    iface_seen: std::cell::Cell::new(0),
                    customized_seen: std::cell::Cell::new(false),
                    identity,
                    real_shape,
                    real_conv,
                },
            );
        }
    });
    leaf.map(|leaf| MhBindHit {
        leaf,
        guard,
        bound,
        direct,
        conv,
        real,
        iface,
        iface_seen: 0,
        customized_seen: false,
        real_conv,
    })
}

/// The interface bind's per-call receiver type guard (JIT round 18 wave 11,
/// lane mhresid; THROWS18-4, `CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE`): is
/// the receiver word `raw` an object whose class implements the interface
/// `iface`? `seen` is the site's last accepted class (one compare for a
/// monomorphic site); a newly accepted class is remembered on the site. An
/// array, an unreadable word or a class that does not implement it is `false`
/// (the door's call). Reads only; the subtype test takes the class manager's
/// read lock only on a memo miss.
pub fn mh_bind_interface_receiver_ok(
    vm: &SharedVm,
    site_key: usize,
    iface: u32,
    seen: u32,
    raw: i64,
) -> bool {
    let bits = raw as u64;
    if bits == 0 || (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return false;
    }
    let Some(obj) = vm.mem.heap.is_object_address(bits as usize) else {
        return false;
    };
    let Some(class) = jit_dispatch_class_validated(vm, obj).class_id() else {
        return false;
    };
    if class.as_u32() == seen {
        return true;
    }
    if !jit_is_subclass_of_cached(vm, class, ClassId::new(iface))
        && !crate::runtime::interpreter::lambda_proxy_satisfies_public(vm, class, ClassId::new(iface))
    {
        return false;
    }
    MH_BIND_SITES.with(|sites| {
        if let Some(site) = sites.borrow().get(&site_key) {
            site.iface_seen.set(class.as_u32());
        }
    });
    true
}



/// Link a COMPILED `MethodHandle.invokeExact` / `invoke` call site the way
/// the interpreter links one at its first execution
/// (`jit_bridge::mh_site_link_caller_types`; JIT round 17 wave 3, lane mh4):
/// every class the site's descriptor names that the caller's loader has not
/// seen yet is resolved through that loader (HotSpot resolves the site's
/// `MethodType` with the caller's loader, JVMS §5.4.3.6, which loads), so
/// the door's `invokeExact` loader check and the constant bind's loader check
/// see the caller's own copy. A method compiled before its first call used to
/// reach the door through this helper only, unlinked (`R17MhLoaders
/// cross-loader-null` compiled first kept the old answer).
///
/// The caller is the site's class (`JitInvokeInfo::declaring_class_id`; `0`,
/// the bootstrap loader, links nothing). The loads run Java code and may
/// collect: the argument buffer's references are pinned across them
/// ([`CompileArgPins`]) and their current addresses written back into the
/// buffer, which the dispatch reads after this. `true` when anything was
/// loaded (the caller re-reads the handle and the epoch). Called on a site's
/// first compiled call per thread ([`mh_constant_bind_redirect`]).
///
/// JIT round 18 wave 11 (lane mhresid; item 4 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): under
/// `CRATONVM_MH_SITE_LINK_NCDFE` (default ON, shared with the interpreted
/// link, `invoke.rs` `mh_site_link_checked`) the first failed load is
/// PROPAGATED, as HotSpot's `find_method_handle_type` does: it is converted
/// into its throwable through the helpers' one conversion
/// ([`stash_helper_failure`]) while the arguments are still pinned, stashed
/// as the pending exception, and [`MhCompiledSiteLink::Threw`] tells the
/// redirect to return the sentinel. With the switch off a failure is
/// ignored, as before.
///
/// # Safety
/// `args_ptr` / `num_args` are the dispatch helper's argument buffer, as
/// compiled code passed it; no `jit_thread_mut` borrow may be live.
#[cold]
pub unsafe fn mh_compiled_site_link(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_ptr: i64,
    num_args: i64,
) -> MhCompiledSiteLink {
    use crate::runtime::interpreter::invoke::mh_site_link_access_refusal;
    use crate::runtime::interpreter::jit_bridge;
    if info.declaring_class_id == 0 || num_args < 1 {
        return MhCompiledSiteLink::Nothing;
    }
    let caller = ClassId::new(info.declaring_class_id);
    let mut missing =
        jit_bridge::mh_site_link_missing_names(vm, info.method_name, info.descriptor, caller);
    // Interpreter round i4 wave 99, lane L4: a `java/` name no loader has
    // loaded yet is loaded too (`invoke.rs`
    // `mh_site_link_add_missing_java_names`), so the access half below judges
    // it (a package-private `java.*` class is HotSpot's `IllegalAccessError`).
    crate::runtime::interpreter::invoke::mh_site_link_add_missing_java_names(
        vm,
        info.method_name,
        info.descriptor,
        caller,
        &mut missing,
    );
    // Interpreter round i4 wave 97, lane L4: the link's access half
    // (`invoke.rs` `mh_site_link_access_refusal`, HotSpot's
    // `check_klass_accessibility` for each class the site's type names), which
    // the interpreted link runs and this one did not: a compiled site naming a
    // package-private class of another runtime package reached the door, whose
    // own copy is asked only for a name two loaders define. Pure reads; a
    // refusal is stashed like a failed load and is not recorded as linked.
    // Only for a USER loader's caller (`invoke.rs`
    // `mh_site_link_caller_is_user_loader`: a built-in loader's answer here
    // comes from the flat store and may be a class it cannot see; w101,
    // `L4W79BootSiteType boot-site`).
    let check_access =
        crate::runtime::interpreter::invoke::mh_site_link_caller_is_user_loader(vm, caller);
    if missing.is_empty() {
        if !check_access {
            return MhCompiledSiteLink::Nothing;
        }
        let Some(refusal) =
            mh_site_link_access_refusal(vm, info.method_name, info.descriptor, caller, &[])
        else {
            return MhCompiledSiteLink::Nothing;
        };
        let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
            return MhCompiledSiteLink::Nothing;
        };
        let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
        let pins = CompileArgPins::pin_raw(vm, thread, info, args);
        let threw = stash_helper_failure(
            vm,
            thread,
            refusal,
            &"MethodHandle call site link failed",
            true,
            true,
        );
        let moved = pins.release_raw(thread, args);
        if let Some(moved) = moved {
            let out = args_ptr as *mut i64;
            for (i, word) in moved.iter().enumerate().take(num_args as usize) {
                *out.add(i) = *word;
            }
        }
        return if threw {
            MhCompiledSiteLink::Threw
        } else {
            MhCompiledSiteLink::Loaded
        };
    }
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return MhCompiledSiteLink::Nothing;
    };
    let propagate = crate::runtime::interpreter::invoke::mh_site_link_ncdfe_enabled();
    let mut threw = false;
    let moved = {
        let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
        let pins = CompileArgPins::pin_raw(vm, thread, info, args);
        let mut resolved: Vec<(&str, ClassId)> = Vec::new();
        for name in &missing {
            let loaded = jit_bridge::mh_site_link_load(
                vm,
                thread,
                info.method_name,
                info.descriptor,
                caller,
                name,
                " (compiled)",
            );
            match loaded {
                Ok(id) => resolved.push((name.as_str(), id)),
                Err(failed) => {
                    if propagate {
                        // Built while the arguments are pinned: the conversion
                        // allocates.
                        threw = stash_helper_failure(
                            vm,
                            thread,
                            failed,
                            &"MethodHandle call site link failed",
                            true,
                            true,
                        );
                        break;
                    }
                }
            }
        }
        // Wave 97: the access half, after the loads (see above).
        if !threw && check_access {
            if let Some(refusal) = mh_site_link_access_refusal(
                vm,
                info.method_name,
                info.descriptor,
                caller,
                &resolved,
            ) {
                threw = stash_helper_failure(
                    vm,
                    thread,
                    refusal,
                    &"MethodHandle call site link failed",
                    true,
                    true,
                );
            }
        }
        pins.release_raw(thread, args)
    };
    if let Some(moved) = moved {
        let out = args_ptr as *mut i64;
        for (i, word) in moved.iter().enumerate().take(num_args as usize) {
            *out.add(i) = *word;
        }
    }
    if threw {
        MhCompiledSiteLink::Threw
    } else {
        MhCompiledSiteLink::Loaded
    }
}

/// What [`mh_compiled_site_link`] did (JIT round 18 wave 11, lane mhresid).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MhCompiledSiteLink {
    /// Nothing to load (or the link is off): the handle and epoch stand.
    Nothing,
    /// Something was loaded (Java ran, a collection may have moved the
    /// arguments): the caller re-reads the handle and the epoch.
    Loaded,
    /// A load failed and its throwable is pending: the caller returns the
    /// `i64::MIN` sentinel.
    Threw,
}

/// The words of the handle at `obj` that a compile-time constant bind relies
/// on (JIT round 17 wave 2, lane mh2; MH7-3): the guard words of
/// `MH_COMPILED_BIND_GUARD_SLOTS`, then its `type`'s. `None` when the object
/// is too narrow for any of them (never an admitted handle). Pure reads; the
/// caller guarantees `obj` is the object at that address in this epoch.
pub fn mh_constant_handle_words(vm: &SharedVm, obj: ObjectRef) -> Option<[u64; 7]> {
    let heap = &vm.mem.heap;
    let width = heap.num_fields(obj);
    let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
    if slots.iter().any(|slot| *slot >= width) {
        return None;
    }
    let type_slot =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, heap.class_id_of(obj), "type")?;
    if type_slot >= width {
        return None;
    }
    let mut words = [0u64; 7];
    for (i, slot) in slots.iter().enumerate() {
        words[i] = mh_bind_slot_key(heap.get_field(obj, *slot));
    }
    words[6] = mh_bind_slot_key(heap.get_field(obj, type_slot));
    Some(words)
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING` -- default ON (see below; built
/// opt-in in JIT round 16 wave 15, lane mh6; proposal MHB-1 of `jit-r16-mhbind-proposals-RETIRED-20261006.md`): a
/// compiled `invokeExact` / `invoke` site whose handle converts `Integer` /
/// `Number` / `Object` arguments into the member's `int` parameters, or its
/// `int` return into such a reference, binds like an exact one; the helper
/// unboxes and boxes around the member's call ([`mh_constant_bind_call`]).
/// Default ON since round 16 wave 17 (w15a A/B: exact-astyped 2950 -> 424 ns,
/// invoke-box 2738 -> 380; 20/20 loaded); `=0` turns it off. Read on the cold
/// fill only ([`mh_constant_bind_fill`]); the answer rides on the bound site,
/// so this getter holds no static.
pub fn mh_constant_bind_converting_flag() -> bool {
    true
}

/// `CRATONVM_DBG_JIT_MH_BIND=1` (JIT round 16 wave 15, lane mh6): one stderr
/// line per cold fill of a `MethodHandle` site ([`mh_constant_bind_dbg_line`]),
/// the engagement record of every constant-bind switch. Read on the cold fill
/// only.
pub fn mh_constant_bind_dbg() -> bool {
    false}

/// The fill's verdict, as `[mh-bind] MethodHandle.<name><site> -> <verdict>`:
/// `door`, or the member and how it is called (`exact`, `bound`, or
/// `converting unbox=<mask> box=<bool>`).
pub fn mh_constant_bind_dbg_line(
    info: &JitInvokeInfo,
    leaf: Option<&'static JitInvokeInfo>,
    bound: bool,
    conv: Option<&MhBindConv>,
) {
    let verdict = match (leaf, conv) {
        (None, _) => "door".to_string(),
        (Some(l), Some(c)) => format!(
            "{}.{}{} converting unbox={:#x} box={}",
            l.class_name, l.method_name, l.descriptor, c.unbox_int, c.box_int_return
        ),
        (Some(l), None) => format!(
            "{}.{}{} {}",
            l.class_name,
            l.method_name,
            l.descriptor,
            if bound {
                "bound"
            } else if l.invoke_kind == 2 {
                // THROWS18-4 (round 18 wave 11, lane mhresid).
                "interface-guarded"
            } else {
                "exact"
            }
        ),
    };
    eprintln!(
        "[mh-bind] MethodHandle.{}{} -> {verdict}",
        info.method_name, info.descriptor
    );
}

/// The warm-path form of a converting admission's answer (MHB-1): the slots of
/// the handle's `type` and of `Integer.value`, both resolved by name as
/// `get_field_by_name` resolves them, and the handle's `type` word now. `None`
/// when either slot does not resolve inside its object.
pub fn mh_bind_conv_of(
    vm: &SharedVm,
    obj: ObjectRef,
    conversion: &cratonvm_native_builtins::lang_invoke::MhBindConversion,
) -> Option<MhBindConv> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    let type_slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "type")?;
    if type_slot >= heap.num_fields(obj) {
        return None;
    }
    let integer = ClassId::new(conversion.integer_class_id);
    let value_slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, integer, "value")?;
    Some(MhBindConv {
        type_slot,
        type_word: mh_bind_slot_key(heap.get_field(obj, type_slot)),
        unbox_int: conversion.unbox_int,
        box_int_return: conversion.box_int_return,
        integer_class_id: conversion.integer_class_id,
        value_slot,
    })
}

/// The warm-path form of a real converting admission's answer (j27 wave 5,
/// lane MHASTYPE): the `value` slot of each loaded wrapper, resolved by name as
/// [`mh_bind_conv_of`] resolves `Integer.value`. `None` when a loaded
/// wrapper's slot does not resolve (the site keeps the door).
pub fn mh_desc_arity(desc: &str) -> Option<u8> {
    let bytes = desc.as_bytes();
    if *bytes.first()? != b'(' {
        return None;
    }
    let mut i = 1usize;
    let mut count = 0u8;
    while i < bytes.len() {
        match bytes[i] {
            b')' => return Some(count),
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                if bytes[i] == b'L' {
                    i = bytes[i..].iter().position(|&b| b == b';')? + i + 1;
                } else {
                    i += 1;
                }
                count = count.checked_add(1)?;
            }
            b'L' => {
                i = bytes[i..].iter().position(|&b| b == b';')? + i + 1;
                count = count.checked_add(1)?;
            }
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
                i += 1;
                count = count.checked_add(1)?;
            }
            _ => return None,
        }
    }
    None
}

pub fn mh_real_convert_compile_row(
    vm: &SharedVm,
    main_kind: u8,
    site_desc: &str,
    rc: &MhRealConv,
) -> Option<cratonvm_jit::compile_request::MhConstantConvertRow> {
    use cratonvm_native_builtins::lang_invoke::{
        MH_REAL_UNBOX_INT, MH_REAL_UNBOX_LONG, MhRealBox,
    };
    let arity = mh_desc_arity(site_desc)?;
    if usize::from(arity) > 8 {
        return None;
    }
    let mut cast_class_ids = [0u32; 8];
    let mut cast_class_names: [String; 8] = Default::default();
    let mut unbox_op = [0u8; 8];
    let mut unbox_class_ids = [0u32; 8];
    {
        let cm = vm.classes.class_manager.read();
        for i in 0..usize::from(arity) {
            if rc.cast[i] != 0 {
                let c = cm.get_class(ClassId::new(rc.cast[i]))?;
                cast_class_ids[i] = rc.cast[i];
                cast_class_names[i] = c.name.to_string();
            }
            if rc.unbox[i] != 0 {
                if (rc.unbox[i] & (1 << MH_REAL_UNBOX_LONG)) != 0 {
                    let id = rc.wrappers[MH_REAL_UNBOX_LONG].0;
                    if id == 0 {
                        return None;
                    }
                    unbox_op[i] = 2;
                    unbox_class_ids[i] = id;
                } else if (rc.unbox[i] & (1 << MH_REAL_UNBOX_INT)) != 0 {
                    let id = rc.wrappers[MH_REAL_UNBOX_INT].0;
                    if id == 0 {
                        return None;
                    }
                    unbox_op[i] = 1;
                    unbox_class_ids[i] = id;
                } else {
                    return None;
                }
            }
        }
    }
    let (box_return, box_class_id) = match rc.box_return {
        MhRealBox::Keep => (0, 0),
        MhRealBox::Int => {
            let id = rc.wrappers[MH_REAL_UNBOX_INT].0;
            if id == 0 {
                return None;
            }
            (1, id)
        }
        MhRealBox::Long => {
            let id = rc.wrappers[MH_REAL_UNBOX_LONG].0;
            if id == 0 {
                return None;
            }
            (2, id)
        }
    };
    Some(cratonvm_jit::compile_request::MhConstantConvertRow {
        main_kind,
        arity,
        cast_class_ids,
        cast_class_names,
        unbox_op,
        unbox_class_ids,
        box_return,
        box_class_id,
    })
}

pub fn mh_real_conv_of(
    vm: &SharedVm,
    conversion: &cratonvm_native_builtins::lang_invoke::MhRealConversion,
) -> Option<MhRealConv> {
    let mut wrappers = [(0u32, 0usize); 5];
    for (k, id) in conversion.wrapper_class_ids.iter().enumerate() {
        if *id != 0 {
            let slot =
                crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, ClassId::new(*id), "value")?;
            wrappers[k] = (*id, slot);
        }
    }
    Some(MhRealConv {
        unbox: conversion.unbox,
        cast: conversion.cast,
        wrappers,
        box_return: conversion.box_return,
    })
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_SHAPE` -- default ON since its A/B (`=0` off; JIT round 16
/// wave 11, lane mhbind; proposal MHF-1 of `jit-r16-mhfloor-proposals-RETIRED-20261006.md`): a
/// bound `invokeExact` site also takes a handle at ANOTHER address whose shape
/// is the bound one's ([`mh_bind_shape_matches`]), so a site that sees a fresh
/// `bindTo` result of one source per call (`R16MhFloor bind-each`) stays bound
/// instead of re-filling until it gives up. `1`/`true`/`on`/`yes` turns it on.
/// Read on the cold fill only ([`mh_constant_bind_fill`]); the answer rides on
/// the bound site, so this getter holds no static.
pub fn mh_constant_bind_shape_flag() -> bool {
    true
}

/// The shape of the handle at `obj` (validated by the fill's
/// `is_object_address`), or `None` when its class has no `type` slot inside
/// it.
pub fn mh_bind_shape_of(vm: &SharedVm, obj: ObjectRef) -> Option<MhBindShape> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    if class_id.as_u32() == 0 {
        return None;
    }
    let width = heap.num_fields(obj);
    let type_slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "type")?;
    let extra = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_SHAPE_SLOTS;
    if type_slot >= width || extra.iter().any(|slot| *slot >= width) {
        return None;
    }
    let mut words = [0u64; 5];
    for (i, slot) in extra.iter().enumerate() {
        words[i] = mh_bind_slot_key(heap.get_field(obj, *slot));
    }
    words[4] = mh_bind_slot_key(heap.get_field(obj, type_slot));
    Some(MhBindShape {
        class_id: class_id.as_u32(),
        width,
        type_slot,
        words,
    })
}

/// Is the handle at `obj` -- at another address than the site's bound one, in
/// the same VM and collection epoch -- of the bound one's shape (MHF-1)? Its
/// class (read through `class_id_of`, which validates the address as an
/// object start: the word is a compiled caller's `MethodHandle` reference, but
/// this is its first header read), its slot count, the guard words of
/// `MH_COMPILED_BIND_GUARD_SLOTS` and the words of `shape`. Every slot the
/// admission (`mh_compiled_bind_target`) reads is among them, and the objects
/// they name are immutable or are the same objects (an equal address in an
/// equal epoch), so the admission would answer this handle as it answered the
/// bound one. A bound receiver is the `MH_BOUND` guard word: the same object.
pub fn mh_bind_shape_matches(
    vm: &SharedVm,
    obj: ObjectRef,
    guard: &[u64; 7],
    shape: &MhBindShape,
) -> bool {
    let heap = &vm.mem.heap;
    if heap.class_id_of(obj).as_u32() != shape.class_id || heap.num_fields(obj) != shape.width {
        return false;
    }
    let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
    if !slots
        .iter()
        .enumerate()
        .all(|(i, slot)| mh_bind_slot_key(heap.get_field(obj, *slot)) == guard[i + 1])
    {
        return false;
    }
    let extra = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_SHAPE_SLOTS;
    extra
        .iter()
        .enumerate()
        .all(|(i, slot)| mh_bind_slot_key(heap.get_field(obj, *slot)) == shape.words[i])
        && mh_bind_slot_key(heap.get_field(obj, shape.type_slot)) == shape.words[4]
}

/// What the redirect's warm path checks on a handle at another address than
/// the site's bound one: a carrier's shape (MHF-1) or a real `bindTo`
/// result's (MHB-3, JIT round 21 wave 17, lane mhbind).
#[derive(Clone, Copy)]
pub enum MhOtherAddress {
    Carrier(MhBindShape),
    RealBound(MhRealBoundShape),
}





/// The [`MhRealBoundShape`] of the real `bindTo` result at `obj` (validated by
/// the fill's `is_object_address`) whose bound value is the word `receiver`,
/// or `None` when a field does not resolve inside the instance or the bound
/// value is not that object. Cold: the fill's.
pub fn mh_real_bound_shape_of(
    vm: &SharedVm,
    obj: ObjectRef,
    receiver: u64,
) -> Option<MhRealBoundShape> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    if class_id.as_u32() == 0 {
        return None;
    }
    let width = heap.num_fields(obj);
    let slot = |name: &str| {
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, name)
            .filter(|s| *s < width)
    };
    let target_slot = slot("argL0")?;
    let receiver_slot = slot("argL1")?;
    let form_slot = slot("form")?;
    let type_slot = slot("type")?;
    if receiver == 0 || mh_bind_slot_key(heap.get_field(obj, receiver_slot)) != receiver {
        return None;
    }
    let robj = heap.is_object_address(receiver as usize)?;
    let receiver_class = jit_dispatch_class_validated(vm, robj).class_id()?;
    Some(MhRealBoundShape {
        class_id: class_id.as_u32(),
        target_slot,
        receiver_slot,
        form_slot,
        type_slot,
        target_word: mh_bind_slot_key(heap.get_field(obj, target_slot)),
        form_word: mh_bind_slot_key(heap.get_field(obj, form_slot)),
        type_word: mh_bind_slot_key(heap.get_field(obj, type_slot)),
        receiver_class: receiver_class.as_u32(),
        receiver_class_alt: 0,
    })
}

/// The `(argL1 slot, bound value's exact class id)` the compile doors need
/// for the REAL `bindTo` result at `obj` (validated by the fill's
/// `is_object_address`), whose bound value is the word `receiver` (MHBIND21-1,
/// JIT round 21 wave 19, lane mhbind2): the slot must lie inside the instance
/// and hold that very word, and the value's class must read. Reads only; on
/// the cold fill.
pub fn mh_real_bound_compile_row(vm: &SharedVm, obj: ObjectRef, receiver: u64) -> Option<(u32, u32)> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    if class_id.as_u32() == 0 || receiver == 0 {
        return None;
    }
    let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "argL1")
        .filter(|s| *s < heap.num_fields(obj))?;
    if mh_bind_slot_key(heap.get_field(obj, slot)) != receiver {
        return None;
    }
    let robj = heap.is_object_address(receiver as usize)?;
    let class = jit_dispatch_class_validated(vm, robj).class_id()?;
    Some((u32::try_from(slot).ok()?, class.as_u32()))
}

/// j27 wave 6 (lane T1LAM2): does the cold fill record a bound STATIC compile
/// row (`invoke_kind 6`)? Only for a REAL handle the bind admitted with a leaf
/// and the compile memo on (`eligible`), no conversion of either kind
/// (`unconverted`: the compile door calls the member as it is), bound, and a
/// static member (`invoke_kind 3`). The virtual bound row (kind 4) is the
/// MHBIND21-1 block's.
pub fn mh_bound_static_compile_row_wanted(
    eligible: bool,
    unconverted: bool,
    bound: bool,
    invoke_kind: u8,
) -> bool {
    eligible && unconverted && bound && invoke_kind == 3
}



pub(in crate::jit) fn ensure_booleans_cached(vm: &SharedVm) -> bool {
    let ids = &vm.jit.reflect_link_ids;
    if ids.boolean_true.load(std::sync::atomic::Ordering::Acquire) != 0
        && ids.boolean_false.load(std::sync::atomic::Ordering::Acquire) != 0
    {
        return true;
    }
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return false;
    };
    let ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    let (bcid, tidx, fidx) = {
        let cm = vm.classes.class_manager.read();
        let Some(bcid) = cm.find_unique_class_by_name("java/lang/Boolean") else {
            return false;
        };
        let Some(cls) = cm.get_class(bcid) else {
            return false;
        };
        let Some((tidx, _)) = cls.find_own_field_by_descriptor("TRUE", "Ljava/lang/Boolean;") else {
            return false;
        };
        let Some((fidx, _)) = cls.find_own_field_by_descriptor("FALSE", "Ljava/lang/Boolean;") else {
            return false;
        };
        (bcid, tidx, fidx)
    };
    if !ctx.is_class_initialized(bcid) {
        return false;
    }
    if let (Value::Object(Some(t)), Value::Object(Some(f))) = (
        ctx.get_static_field(bcid, tidx),
        ctx.get_static_field(bcid, fidx),
    ) {
        ids.boolean_true
            .store(t.as_ptr() as u64, std::sync::atomic::Ordering::Release);
        ids.boolean_false
            .store(f.as_ptr() as u64, std::sync::atomic::Ordering::Release);
        true
    } else {
        false
    }
}

pub(in crate::jit) fn publish_reflect_invoke_cache(
    cache: &cratonvm_jit::ReflectInvokeCache,
    vm: &SharedVm,
    leaf: &'static JitInvokeInfo,
    handle_bits: u64,
    receiver_word: i64,
    caller_cid: u32,
    ret: u8,
) {
    if !matches!(ret, b'V' | b'L' | b'[' | b'Z' | b'I')
        || reflect_link_declines_for_hidden_frames(vm)
    {
        return;
    }
    if ret == b'Z' && !ensure_booleans_cached(vm) {
        return;
    }
    let leaf_ptr = leaf as *const JitInvokeInfo as usize;
    let key = jit_site_key(vm.vm_identity, leaf_ptr);
    if site_quiesced_for_redefinition(vm, leaf, key)
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return;
    }
    let statically_bound = matches!(leaf.invoke_kind, 1 | 3);
    let found = if statically_bound {
        DISPATCH_CACHE.with(|dc| {
            dc.borrow()
                .get(&key)
                .filter(|c| !c.names_a_stale_body())
                .map(|c| (c.entry, c.needs_context, 0u32))
        })
    } else if receiver_word != 0 {
        let receiver = unsafe { ObjectRef::from_raw(receiver_word as usize as *mut u8) };
        jit_dispatch_class(vm, receiver)
            .class_id()
            .and_then(|rcid| {
                VIRTUAL_DISPATCH_CACHE.with(|dc| {
                    dc.borrow()
                        .get(&(key, rcid.as_u32()))
                        .filter(|c| !c.names_a_stale_body())
                        .map(|c| (c.entry, c.needs_context, rcid.as_u32()))
                })
            })
    } else {
        None
    };
    let resolved = found.or_else(|| {
        if statically_bound {
            crate::runtime::interpreter::jit_bridge::try_jit_compile_callee_for_class_on_miss(
                vm,
                ClassId::new(leaf.declaring_class_id),
                leaf.class_name,
                leaf.method_name,
                leaf.descriptor,
                false,
                virtual_dispatch_counter_door_miss(),
            )
            .map(|(_pin, e, nc)| (e, nc, 0u32))
        } else if receiver_word != 0 {
            let receiver = unsafe { ObjectRef::from_raw(receiver_word as usize as *mut u8) };
            let rcid = jit_dispatch_class(vm, receiver).class_id()?;
            let cname = vm
                .classes
                .class_manager
                .read()
                .get_class(rcid)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| leaf.class_name.into());
            probe_selected_declaring_body(vm, leaf, rcid)
                .or_else(|| {
                    crate::runtime::interpreter::jit_bridge::try_jit_compile_callee_for_class_on_miss(
                        vm,
                        rcid,
                        &cname,
                        leaf.method_name,
                        leaf.descriptor,
                        true,
                        virtual_dispatch_counter_door_miss(),
                    )
                })
                .map(|(_pin, e, nc)| (e, nc, rcid.as_u32()))
        } else {
            None
        }
    });
    let Some((entry, needs_ctx, rcid)) = resolved else {
        return;
    };
    if entry == 0 {
        return;
    }
    let epoch = vm
        .jit
        .reflect_link_ids
        .gc_epoch
        .load(std::sync::atomic::Ordering::Acquire);
    let slot_idx = cratonvm_jit::ReflectInvokeCache::slot_index(handle_bits);
    let slot = &cache.entries[slot_idx];
    let cur = slot
        .epoch_and_meta
        .load(std::sync::atomic::Ordering::Acquire);
    if (cur >> 16) != epoch || (cur & 1) == 0 {
        let ret_kind = match ret {
            b'V' => cratonvm_jit::REFLECT_RET_VOID,
            b'Z' => cratonvm_jit::REFLECT_RET_BOOL,
            b'I' => cratonvm_jit::REFLECT_RET_INT,
            _ => cratonvm_jit::REFLECT_RET_OBJ,
        };
        let is_static_bit = if leaf.invoke_kind == 3 { 2u64 } else { 0u64 };
        let meta = (epoch << 16) | ((ret_kind as u64) << 8) | is_static_bit | 1u64;
        slot.handle_bits
            .store(handle_bits, std::sync::atomic::Ordering::Release);
        slot.receiver_cid
            .store(rcid, std::sync::atomic::Ordering::Release);
        slot.caller_cid
            .store(caller_cid, std::sync::atomic::Ordering::Release);
        slot.entry_ptr_tagged.store(
            (entry as u64) | if needs_ctx { 1 } else { 0 },
            std::sync::atomic::Ordering::Release,
        );
        slot.epoch_and_meta
            .store(meta, std::sync::atomic::Ordering::Release);
    }
}

pub unsafe fn try_populate_method_invoke_cache(
    vm_ptr: i64,
    info: &JitInvokeInfo,
    args_ptr: i64,
) {
    if vm_ptr == 0 || args_ptr == 0 || info.declaring_class_id == 0 {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if reflect_link_declines_for_hidden_frames(vm) {
        return;
    }
    let args = std::slice::from_raw_parts(args_ptr as *const i64, 3);
    if args[0] == 0 || args[2] != 0 {
        return;
    }
    let Some(method_ref) = mh_link_object(vm, args[0]) else {
        return;
    };
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return;
    };
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    let Some(dmha_cid) = reflect_link_class_id(
        &ctx,
        &vm.jit.reflect_link_ids.accessors[0],
        REFLECT_ACCESSOR_CLASS,
    ) else {
        return;
    };
    let Value::Object(Some(accessor)) = ctx.get_field_by_name(method_ref, "methodAccessor") else {
        return;
    };
    if ctx.class_id_of_object(accessor).as_u32() != dmha_cid {
        return;
    }
    let Value::Int(0) = ctx.get_field_by_name(accessor, "paramCount") else {
        return;
    };
    let Value::Object(Some(target)) = ctx.get_field_by_name(accessor, "target") else {
        return;
    };
    let Some(member_obj) = cratonvm_native_builtins::lang_invoke::mh_reflect_accessor_member(
        &mut ctx,
        target,
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ) else {
        return;
    };
    let Some(member) = cratonvm_native_builtins::mh_compiled_member(&mut ctx, member_obj) else {
        return;
    };
    let kind = match member.ref_kind() {
        6 => 3u8,
        5 => 0u8,
        9 => 2u8,
        7 if cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_special_enabled() => 1u8,
        _ => return,
    };
    if member.param_count() != 0 {
        return;
    }
    let Some(leaf) = mh_link_leaf(vm, &mut ctx, &member, kind) else {
        return;
    };
    let mut ptypes = [""; 3];
    let Some((0, ret)) = reflect_accessor_split(leaf.descriptor, &mut ptypes) else {
        return;
    };
    publish_reflect_invoke_cache(
        &vm.jit.reflect_link_ids.method_invoke_cache,
        vm,
        leaf,
        args[0] as u64,
        args[1],
        info.declaring_class_id,
        ret,
    );
}

pub unsafe extern "C" fn jit_reflect_method_invoke_sentinel(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    if vm_ptr == 0 || info_ptr == 0 {
        crate::jit::helpers::set_jit_deopt_pending();
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let exc_opt = {
        let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
            crate::jit::helpers::set_jit_deopt_pending();
            return i64::MIN;
        };
        take_jit_pending_exception(thread)
    };
    if let Some(exc) = exc_opt {
        if let Some((thread, _guard)) = jit_thread_mut() {
            let ite_cid = {
                let cm = vm.classes.class_manager.read();
                cm.find_unique_class_by_name("java/lang/reflect/InvocationTargetException")
            };
            if let Some(ite_cid) = ite_cid {
                let nf = vm.classes.class_manager.read().get_class(ite_cid).map(|c| c.num_total_fields.max(1)).unwrap_or(1);
                if let Ok(ite_obj) = crate::runtime::interpreter::gc_and_alloc::alloc_object_shared(vm, thread, ite_cid, nf) {
                    let _ = crate::runtime::interpreter::execute(
                        vm,
                        thread,
                        ite_cid,
                        "<init>",
                        "(Ljava/lang/Throwable;)V",
                        &[Value::Object(Some(ite_obj)), Value::Object(Some(exc))],
                    );
                    set_jit_pending_exception(thread, ite_obj);
                    return i64::MIN;
                }
            }
            set_jit_pending_exception(thread, exc);
        }
        return i64::MIN;
    }
    crate::jit::helpers::dispatch::jit_invoke_dispatch_body::jit_invoke_dispatch_body(
        vm_ptr,
        info_ptr,
        args_ptr,
        num_args,
    )
}
