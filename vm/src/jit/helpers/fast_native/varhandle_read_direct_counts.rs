#[allow(unused_imports)]
use super::*;

pub const VARHANDLE_READ_DESCRIPTORS: [&str; cratonvm_jit::VARHANDLE_READ_KINDS] = [
    "(Ljava/lang/Object;)Z",
    "(Ljava/lang/Object;)B",
    "(Ljava/lang/Object;)C",
    "(Ljava/lang/Object;)S",
    "(Ljava/lang/Object;)I",
    "(Ljava/lang/Object;)J",
    "(Ljava/lang/Object;)F",
    "(Ljava/lang/Object;)D",
    // REF_OBJECT. Not a stand-in at all: a site of this kind declares exactly
    // this descriptor, so the cold arm dispatches with the real one.
    "(Ljava/lang/Object;)Ljava/lang/Object;",
    // REF_STRICT. Here it IS erased, and the one reader that can tell the
    // difference is `unbox_poly_return_checked`'s W6-1 rule. That rule is
    // reproduced for this kind in `varhandle_read_direct_impl`, which needs no
    // descriptor: the kind exists precisely because NO boxed primitive is
    // assignable to what such a site declared.
    "(Ljava/lang/Object;)Ljava/lang/Object;",
];

/// Synthetic call sites for the cold arm of the `VarHandle` read helpers, one
/// per slot. `static`, not a stack temporary: `jit_invoke_dispatch` keys its
/// per-site memos on `(vm_identity, info address)`, so the address has to be
/// process-stable — the same contract `INTEGER_INT_VALUE_INFO` and
/// `PRECONDITIONS_CHECK_INDEX_INFO` are statics for.
pub static VARHANDLE_READ_INFOS: [JitInvokeInfo; cratonvm_jit::VARHANDLE_READ_SLOTS] = [
    vh_read_info(0, 0),
    vh_read_info(0, 1),
    vh_read_info(0, 2),
    vh_read_info(0, 3),
    vh_read_info(0, 4),
    vh_read_info(0, 5),
    vh_read_info(0, 6),
    vh_read_info(0, 7),
    vh_read_info(0, 8),
    vh_read_info(0, 9),
    vh_read_info(1, 0),
    vh_read_info(1, 1),
    vh_read_info(1, 2),
    vh_read_info(1, 3),
    vh_read_info(1, 4),
    vh_read_info(1, 5),
    vh_read_info(1, 6),
    vh_read_info(1, 7),
    vh_read_info(1, 8),
    vh_read_info(1, 9),
    vh_read_info(2, 0),
    vh_read_info(2, 1),
    vh_read_info(2, 2),
    vh_read_info(2, 3),
    vh_read_info(2, 4),
    vh_read_info(2, 5),
    vh_read_info(2, 6),
    vh_read_info(2, 7),
    vh_read_info(2, 8),
    vh_read_info(2, 9),
    vh_read_info(3, 0),
    vh_read_info(3, 1),
    vh_read_info(3, 2),
    vh_read_info(3, 3),
    vh_read_info(3, 4),
    vh_read_info(3, 5),
    vh_read_info(3, 6),
    vh_read_info(3, 7),
    vh_read_info(3, 8),
    vh_read_info(3, 9),
];

/// One entry of [`VARHANDLE_READ_INFOS`]. `num_jit_args: 2` counts the
/// receiver — the `VarHandle` itself — plus the single coordinate, matching
/// `INTEGER_INT_VALUE_INFO`'s `1` for a zero-argument `invokevirtual`.
pub const fn vh_read_info(mode: usize, kind: usize) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/VarHandle",
        method_name: cratonvm_jit::VARHANDLE_READ_MODES[mode],
        descriptor: VARHANDLE_READ_DESCRIPTORS[kind],
        num_jit_args: 2,
        return_type: cratonvm_jit::varhandle_read_slot_return(
            mode * cratonvm_jit::VARHANDLE_READ_KINDS + kind,
        ),
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

/// `VarHandle` reads served by a thin direct call, and the ones that had to be
/// handed back to the generic dispatcher.
///
/// Both halves are needed, and a run with a non-zero site count and a zero hit
/// count is the specific failure this pair exists to name: the bind landed at
/// compile time and every execution declined. `declines` non-zero with `hits`
/// non-zero is normal — the first call on a handle resolves its slot inside the
/// native, so it declines and the second call qualifies.
///
/// Counted per thread (r9-vmside) — see [`JitCounterBlock::vh_direct_served`].
///
/// `(served, declined)` counts for the `VarHandle` read direct helpers.
pub fn varhandle_read_direct_counts() -> (u64, u64) {
    vh_direct_counts(VH_DIRECT_READ)
}

/// A complete primitive-read proof. Addresses are weak, never roots: their
/// validity is bounded by the same VM/heap epoch as `DIRECT_RECEIVER_MEMO`.
/// Metadata changes (including weak-key release and handle movement) and
/// layout changes separately invalidate the plan and both byte offsets.
#[derive(Clone, Copy)]
pub struct VarHandlePrimitiveReadProof {
    pub vm: usize,
    pub vh: usize,
    pub receiver: usize,
    pub heap_epoch: u64,
    pub metadata_generation: u64,
    pub layout_generation: u64,
    pub vh_class: u32,
    pub vh_compact: bool,
    pub receiver_class: u32,
    pub site_return: u8,
    pub judged: bool,
    pub exact_slot: Option<usize>,
    pub exact_offset: Option<usize>,
    pub value_offset: usize,
    pub storage: cratonvm_types::FieldStorageKind,
}

pub const VH_PRIMITIVE_READ_PROOF_SLOTS: usize = 64;
thread_local! {
    pub static VH_PRIMITIVE_READ_PROOFS:
        [std::cell::Cell<Option<VarHandlePrimitiveReadProof>>; VH_PRIMITIVE_READ_PROOF_SLOTS] =
        const { [const { std::cell::Cell::new(None) }; VH_PRIMITIVE_READ_PROOF_SLOTS] };
    pub static VH_PRIMITIVE_READ_PROOF_REPORTED_VM: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[inline]
pub fn vh_primitive_read_proof_slot(vh: usize, receiver: usize, ret: u8) -> usize {
    ((vh >> 4).wrapping_mul(31) ^ (receiver >> 4) ^ usize::from(ret))
        & (VH_PRIMITIVE_READ_PROOF_SLOTS - 1)
}

/// The hit cannot allocate or safepoint. Forwarding remains a live read on
/// both objects; their storage bit and class must still agree. The exact
/// flag is read on every judged hit, so even a changed final field cannot
/// turn an exact handle into an unchecked access.
// SAFETY: Performs raw field reads within verified bounds without safepoint or allocation.
pub unsafe fn varhandle_cached_primitive_read(
    vm: &SharedVm,
    vh: usize,
    receiver: usize,
    ret: u8,
    judged: bool,
) -> Option<i64> {
    let slot = vh_primitive_read_proof_slot(vh, receiver, ret);
    let proof = VH_PRIMITIVE_READ_PROOFS.with(|proofs| proofs[slot].get())?;
    if proof.vm != vm.vm_identity
        || proof.vh != vh
        || proof.receiver != receiver
        || proof.site_return != ret
        || proof.judged != judged
        || proof.heap_epoch != vm.mem.address_epoch()
        || proof.metadata_generation
            != cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation()
        || proof.layout_generation != cratonvm_types::layout_generation()
    {
        return None;
    }
    // Keep the heap's checked forwarding path here. The memo's epoch bounds
    // reuse, but it is not the no-intervening-safepoint contract required by
    // load_and_forward_validated.
    let handle = vm
        .mem
        .heap
        .load_and_forward(ObjectRef::from_raw(vh as *mut u8));
    let object = vm
        .mem
        .heap
        .load_and_forward(ObjectRef::from_raw(receiver as *mut u8));
    for (object, class, compact) in [
        (handle, proof.vh_class, proof.vh_compact),
        (object, proof.receiver_class, true),
    ] {
        let ptr = object.as_ptr();
        let live_compact = std::ptr::read(ptr.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET))
            & cratonvm_types::GC_FLAG_COMPACT
            != 0;
        if std::ptr::read(ptr.cast::<u32>()) != class || live_compact != compact {
            return None;
        }
    }
    if judged {
        if crate::runtime::interpreter::var_handle_exact_gate(vm) != proof.exact_slot {
            return None;
        }
        if let Some(offset) = proof.exact_offset {
            let flag = if proof.vh_compact {
                cratonvm_types::read_compact_field(
                    handle.as_ptr().add(offset),
                    cratonvm_types::FieldStorageKind::Boolean,
                    std::sync::atomic::Ordering::Acquire,
                )
            } else {
                // Legacy native-created handles have an atomic Value cell.
                // Recheck its bounds and tag, never cache the flag's value.
                let slot = proof.exact_slot?;
                if !jit_putfield_slot_in_bounds(handle.as_ptr() as i64, slot as i64) {
                    return None;
                }
                cratonvm_types::read_value_checked_atomic(
                    handle.as_ptr().add(offset).cast::<Value>(),
                )?
            };
            if !matches!(flag, Value::Int(0)) {
                return None;
            }
        } else if let Some(slot) = proof.exact_slot {
            // Native-created handles may retain legacy storage. Its live
            // accessor also preserves the synthetic slot's Value semantics.
            if crate::runtime::interpreter::var_handle_receiver_is_exact(vm, handle, slot) {
                return None;
            }
        }
    }
    let value = cratonvm_types::read_compact_field(
        object.as_ptr().add(proof.value_offset),
        proof.storage,
        std::sync::atomic::Ordering::Acquire,
    );
    let bits = match value {
        Value::Int(v) => v as i64,
        Value::Long(v) => v,
        Value::Float(v) => v.to_bits() as i64,
        Value::Double(v) => v.to_bits() as i64,
        _ => return None,
    };
    if crate::runtime::env_cache::dbg_jitc() {
        VH_PRIMITIVE_READ_PROOF_REPORTED_VM.with(|reported| {
            if reported.replace(vm.vm_identity) != vm.vm_identity {
                eprintln!(
                    "[cratonvm-jitc] primitive-varhandle-read cached vm={} ret={}",
                    vm.vm_identity, ret as char
                );
            }
        });
    }
    Some(bits)
}

/// Publish only after the ordinary path served this access. Re-derive the
/// complete proof under one unchanged generation tuple. Legacy field receivers,
/// references and unresolved metadata decline. Legacy handles retain the live
/// exact-flag accessor instead of a compact boolean offset.
/// No Java call, allocation, safepoint or mutable thread borrow occurs here.
// SAFETY: Reads verified receiver and handle metadata without allocation, safepoint, or thread mutation.
pub unsafe fn remember_varhandle_primitive_read(
    vm: &SharedVm,
    vh: usize,
    receiver: usize,
    ret: u8,
    judged: bool,
) -> Option<()> {
    let heap_epoch = vm.mem.address_epoch();
    let metadata_generation =
        cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation();
    let layout_generation = cratonvm_types::layout_generation();
    let (vh_class, key) = direct_receiver_facts(vm, vh)?;
    let (receiver_class, _) = direct_receiver_facts(vm, receiver)?;
    if vh_class == 0 || receiver_class == 0 {
        return None;
    }
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle(
        vm.vm_identity,
        key,
        vh,
    )?;
    if plan.value_desc != ret
        || matches!(ret, b'L' | b'[')
        || !varhandle_plan_admits_receiver(vm, &plan, receiver_class)
    {
        return None;
    }
    let handle = vm
        .mem
        .heap
        .load_and_forward(ObjectRef::from_raw(vh as *mut u8));
    let object = vm
        .mem
        .heap
        .load_and_forward(ObjectRef::from_raw(receiver as *mut u8));
    let vh_compact = std::ptr::read(handle.as_ptr().add(cratonvm_types::GC_FLAGS_BYTE_OFFSET))
        & cratonvm_types::GC_FLAG_COMPACT
        != 0;
    let (value_offset, storage) =
        jit_compact_field_slot(object.as_ptr() as i64, i64::from(plan.field_index))?;
    if Some(storage) != cratonvm_types::FieldStorageKind::from_descriptor_byte(ret) {
        return None;
    }
    let exact_slot = if judged {
        crate::runtime::interpreter::var_handle_exact_gate(vm)
    } else {
        None
    };
    let exact_offset = match exact_slot {
        Some(slot) if vh_compact => {
            let (offset, kind) = jit_compact_field_slot(handle.as_ptr() as i64, slot as i64)?;
            if kind != cratonvm_types::FieldStorageKind::Boolean {
                return None;
            }
            let flag = cratonvm_types::read_compact_field(
                handle.as_ptr().add(offset),
                kind,
                std::sync::atomic::Ordering::Acquire,
            );
            if !matches!(flag, Value::Int(0)) {
                return None;
            }
            Some(offset)
        }
        Some(slot) => {
            if !jit_putfield_slot_in_bounds(handle.as_ptr() as i64, slot as i64) {
                return None;
            }
            let offset = HEADER_SIZE.checked_add(slot.checked_mul(SLOT_SIZE)?)?;
            let flag = cratonvm_types::read_value_checked_atomic(
                handle.as_ptr().add(offset).cast::<Value>(),
            )?;
            // Non-Int synthetic cells keep their existing checked accessor.
            if !matches!(flag, Value::Int(0)) {
                return None;
            }
            Some(offset)
        }
        None => None,    };
    if heap_epoch != vm.mem.address_epoch()
        || metadata_generation
            != cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation()
        || layout_generation != cratonvm_types::layout_generation()
    {
        return None;
    }
    VH_PRIMITIVE_READ_PROOFS.with(|proofs| {
        proofs[vh_primitive_read_proof_slot(vh, receiver, ret)].set(Some(
            VarHandlePrimitiveReadProof {
                vm: vm.vm_identity,
                vh,
                receiver,
                heap_epoch,
                metadata_generation,
                layout_generation,
                vh_class,
                vh_compact,
                receiver_class,
                site_return: ret,
                judged,
                exact_slot,
                exact_offset,
                value_offset,
                storage,
            },
        ));
    });
    Some(())
}

/// Body of every [`VARHANDLE_READ_DIRECT_FNS`] slot.
///
/// Fast path: the handle describes a resolved instance field whose kind agrees
/// with the call site's return, and the answer is one field load. Everything
/// else — a null handle or coordinate, an unresolved or non-instance-field
/// handle, a kind disagreement — falls through to the generic dispatcher with
/// this slot's synthetic call site, so the value, the exception and the
/// `VarHandle` access-mode rules are the registered native's rather than a
/// copy of them.
///
// SAFETY: Called only from JIT-compiled code, with a `vm_ptr` compiled against this
// live VM and two raw references the compiled frame is holding.
pub unsafe fn varhandle_read_direct_impl(
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
    slot: usize,
    site: *const JitInvokeInfo,
    cache_primitive: bool,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    // Round 15 wave 4 (lane vh6): see `note_thin_varhandle_helper_unjudged`.
    // A judged bind (round 15 wave 6) judges, so it is never that line.
    if site.is_null() && crate::runtime::env_cache::dbg_mh_stack() {
        note_thin_varhandle_helper_unjudged(vm, vh, "read", slot);
    }
    let site_ret = cratonvm_jit::varhandle_read_slot_return(slot);
    if cratonvm_jit::varhandle_read_slot_is_reference(slot) {
        let fast = varhandle_thin_fast_arm_admitted(vm, vh, site);
        return varhandle_read_direct_reference(vm, vm_ptr, vh, receiver, slot, site, fast);
    }
    if cache_primitive && vh != 0 && receiver != 0 {
        if let Some(bits) = varhandle_cached_primitive_read(
            vm,
            vh as usize,
            receiver as usize,
            site_ret,
            !site.is_null(),
        ) {
            note_vh_direct(VH_DIRECT_READ, true);
            note_funnel_bypass_hit();
            return bits;
        }
    }
    // The cache miss retains the ordinary live validation and cold dispatcher.
    let fast = varhandle_thin_fast_arm_admitted(vm, vh, site);
    // No SATB flush and no reference-argument forwarding, unlike
    // `jit_invoke_dispatch`: this arm cannot allocate, cannot reach a
    // safepoint, and returns a primitive. It also takes no thread borrow —
    // a primitive result needs no `native_pending_return` handoff root, which
    // is the only thing the funnel's copy of this read wants a thread for.
    // The cold arm below does all of that, because it IS the funnel.
    if fast && vh != 0 && receiver != 0 {
        if let Some(bits) =
            varhandle_instance_field_read_bits(vm, vh as u64, receiver as u64, site_ret, None)
        {
            if cache_primitive {
                let _ = remember_varhandle_primitive_read(
                    vm,
                    vh as usize,
                    receiver as usize,
                    site_ret,
                    !site.is_null(),
                );
            }
            note_vh_direct(VH_DIRECT_READ, true);
            note_funnel_bypass_hit();
            return bits;
        }
    }
    note_vh_direct(VH_DIRECT_READ, false);
    let args = [vh, receiver];
    jit_invoke_dispatch(
        vm_ptr,
        varhandle_thin_cold_arm_info(site, &VARHANDLE_READ_INFOS[slot]),
        args.as_ptr() as i64,
        2,
    )
}

/// The REFERENCE half of [`varhandle_read_direct_impl`], split out because it
/// differs from the primitive half in the two places a reference result costs
/// something.
///
/// **The handoff root.** A reference the callee hands back is published in
/// `native_pending_return` while the helper can still reach a GC point, and
/// the extern wrapper drops it once the value is handed to compiled code
/// (`drop_native_return_handed_to_compiled_code`, handoff-w36b). The cold
/// arm's `jit_invoke_dispatch` has already handed its result off, so the W6-1
/// check, which allocates, pins that result itself. That is what the primitive arm's comment means when it says
/// it takes no thread borrow: it does not need one, and this arm does.
/// `jit_thread_mut` here is the same borrow `jit_invoke_dispatch` takes one
/// frame further in, so the funnel round trip this bind removes was paying for
/// it anyway.
///
/// **W6-1 on the cold arm.** `REF_STRICT` names a site whose declared return
/// type NO boxed primitive is assignable to (the classification is
/// `cratonvm_jit::varhandle_read_helper_slot`'s, and it declines the thirteen
/// types where the answer would depend on which wrapper arrived). So when the
/// cold arm's dispatch produces a box, that is a `WrongMethodTypeException`
/// with no further information needed — which is exactly what
/// `unbox_poly_return_checked` would have raised for the site's real
/// descriptor, and the only observable this bind's erased stand-in would
/// otherwise have lost.
///
/// The FAST arm needs no such check: `varhandle_instance_field_read_bits`
/// refuses a reference site over a primitive variable, and a reference variable
/// holding a box is served by the funnel's own copy of this read today with no
/// check either.
///
// SAFETY: See [`varhandle_read_direct_impl`].
pub unsafe fn varhandle_read_direct_reference(
    vm: &SharedVm,
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
    slot: usize,
    site: *const JitInvokeInfo,
    fast: bool,
) -> i64 {
    if fast && vh != 0 && receiver != 0 {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(bits) = varhandle_instance_field_read_bits(
                vm,
                vh as u64,
                receiver as u64,
                b'L',
                Some(thread),
            ) {
                note_vh_direct(VH_DIRECT_READ, true);
                note_funnel_bypass_hit();
                return bits;
            }
        }
        // The borrow and its guard end here, before the cold arm below takes
        // its own inside `jit_invoke_dispatch`.
    }
    note_vh_direct(VH_DIRECT_READ, false);
    let args = [vh, receiver];
    let bits = jit_invoke_dispatch(
        vm_ptr,
        varhandle_thin_cold_arm_info(site, &VARHANDLE_READ_INFOS[slot]),
        args.as_ptr() as i64,
        2,
    );
    if !site.is_null()
        || cratonvm_jit::varhandle_read_slot_kind(slot)
            != cratonvm_jit::VARHANDLE_READ_KIND_REF_STRICT
        || bits == i64::MIN
        || bits == 0
    {
        // A judged bind dispatched with the site's real descriptor, which
        // applies W6-1 itself (round 15 wave 6). Otherwise: not the strict
        // kind, the dispatch already threw (`i64::MIN` is the deopt sentinel
        // and is not a representable object address), or the access answered
        // `null` — which W6-1 leaves alone by construction.
        return bits;
    }
    varhandle_strict_reference_return_check(vm, slot, bits)
}

/// `unbox_poly_return_checked`'s W6-1 rule, reproduced for a `REF_STRICT` slot
/// whose cold arm has just produced `bits`.
///
/// Returns `bits` unchanged for everything outside the fire set — a value that
/// is not a live object, a class the manager cannot name, and any object that
/// is not one of the eight primitive wrappers. Those are the same exclusions
/// `varhandle_reference_return_mismatch` makes, and for the same reason: a
/// synthetic stand-in, an un-nameable fabricated class or a genuine reference
/// value keeps today's behaviour.
///
// SAFETY: `bits` is whatever `jit_invoke_dispatch` returned for a reference-returning
// site; it is heap-validated here before any dereference.
pub unsafe fn varhandle_strict_reference_return_check(vm: &SharedVm, slot: usize, bits: i64) -> i64 {
    if !crate::vm::vm_exec::vh_strict_reference_return() {
        return bits;
    }
    // Interpreter round i6 wave 132, lane L4 (proposal
    // `i57-L4-proposal-retire-the-w6-1-varhandle-return-text`): the interpreter
    // stopped asking W6-1 under `VmConfig::vh_w6_1_retired` (wave 58, default
    // on), and this compiled twin kept raising it, so a compiled caller could
    // see CratonVM's own `WrongMethodTypeException` text where the interpreted
    // one did not. Retired, it is evaluated only under the census
    // (`CRATONVM_DBG_FIELD_SITE`), where a would-be refusal is counted in
    // `site_stats::VH_W6_1_REFUSAL`, as the interpreter's is, and never raised.
    // `CRATONVM_VH_W6_1_RETIRED=0` restores the rule in both tiers.
    let retired = true;
    if retired && !crate::runtime::interpreter::site_cache::site_stats::on() {
        return bits;
    }
    let Some(obj) = vm.mem.heap.is_object_address(bits as usize) else {
        return bits;
    };
    // An array is never a boxed primitive, though its header names its
    // component (`Integer[]` -> `Integer`); keep today's behaviour for it, as
    // for any other genuine reference (`jit_dispatch_class`).
    let Some(class_id) = jit_dispatch_class_validated(vm, obj).class_id() else {
        return bits;
    };
    let actual = {
        // Scoped: `create_exception_object` below takes the class-manager WRITE
        // lock to load the throwable and must not find this read guard held —
        // the same confinement `varhandle_reference_return_mismatch` documents.
        let cm = vm.classes.class_manager.read();
        match cm.get_class(class_id).map(|c| c.name.to_string()) {
            Some(name) => name,
            None => return bits,
        }
    };
    if !crate::vm::vm_exec::is_primitive_wrapper_class_name(&actual) {
        return bits;
    }
    if retired {
        crate::runtime::interpreter::site_cache::site_stats::bump(
            crate::runtime::interpreter::site_cache::site_stats::VH_W6_1_REFUSAL,
        );
        eprintln!("[VH_W6_1] would refuse (compiled REF_STRICT slot {slot}): produced {actual}");
        return bits;
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return bits;
    };
    // Not the funnel's wording, because this arm genuinely does not have the
    // site's descriptor to quote — it has the fact that MADE the descriptor
    // unnecessary. Saying that is more useful than quoting the erased stand-in,
    // which would name `java/lang/Object` and be a lie.
    let message = format!(
        "VarHandle access site declares a reference return type that no boxed \
         primitive satisfies, but the access produced {}",
        actual.replace('/', ".")
    );
    // `bits` came back from `jit_invoke_dispatch`, which no longer leaves it
    // rooted in `native_pending_return` (handoff-w36b), and the throwable below
    // allocates. Only the `Err` arm hands `bits` on, but that arm is exactly
    // the one where the allocation failed and a collection may have run, so
    // pin it (scanned and remapped; the throwable's constructor may run Java
    // and natives, which is why not the handoff slot) and hand back its
    // current address.
    let pin_idx = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj);
    let created = crate::runtime::exceptions::create_exception_object(
        vm,
        thread,
        "java/lang/invoke/WrongMethodTypeException",
        Some(&message),
    );
    let current = thread.native_pin_roots.get(pin_idx).copied().unwrap_or(obj);
    thread.native_pin_roots.truncate(pin_idx);
    match created {
        Ok(exc) => handle_jit_dispatch_error(
            vm,
            thread,
            crate::error::MethodCallFailed::ExceptionThrown(exc),
            &VARHANDLE_READ_INFOS[slot],
        ),
        // Exception construction failed; the legacy silent path is still
        // better than a fabricated sentinel with nothing behind it.
        Err(_) => current.as_ptr() as i64,
    }
}

/// The thin direct-call target for slot `SLOT` of
/// `cratonvm_jit::VARHANDLE_READ_DIRECT_FNS`.
///
/// One monomorphisation per slot, so the slot — and with it the site's return
/// kind and the synthetic call site the cold arm uses — is a compile-time
/// constant in the emitted `CALL`'s target rather than an argument the JIT has
/// no way to pass (a `JitDirectCall` carries no site constant).
///
/// # SAFETY
///
/// See [`varhandle_read_direct_impl`].
pub unsafe extern "C" fn jit_varhandle_read_direct<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
) -> i64 {
    // Round 15 wave 6 (lane vh8): a judged bind's first word is its site.
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let f = || varhandle_read_direct_impl(vm_ptr, vh, receiver, SLOT, site, true);
    let bits = contain(
        "jit_varhandle_read_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Diagnostic withdrawal: the same helper ABI and checked cold path, with
/// no complete primitive-read proof retained between calls.
// SAFETY: Called under `contain` panic guard from JIT code with valid ABI arguments.
pub unsafe extern "C" fn jit_varhandle_read_direct_uncached<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
) -> i64 {
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let f = || varhandle_read_direct_impl(vm_ptr, vh, receiver, SLOT, site, false);
    let bits = contain(
        "jit_varhandle_read_direct_uncached",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    native_return_handed_to_compiled_code();
    bits
}

/// Addresses of every [`jit_varhandle_read_direct`] monomorphisation, in slot
/// order.
pub fn varhandle_read_direct_fns() -> [usize; cratonvm_jit::VARHANDLE_READ_SLOTS] {
    // Selected under the VM's flag snapshot at compilation, never polled on
    // the hot read path. No registration, native kind or ABI changes.
    let cached =
        true;
    macro_rules! slots {
        ($($slot:literal),* $(,)?) => {
            [$( if cached {
                jit_varhandle_read_direct::<$slot> as *const () as usize
            } else {
                jit_varhandle_read_direct_uncached::<$slot> as *const () as usize
            } ),*]
        };
    }
    slots!(
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39,
    )
}

/// The thin direct-call helper address for one slot, for the OSR door — which
/// takes helper addresses directly rather than through the jit-crate cells,
/// because it can run before `build_helpers` has published them.
pub fn varhandle_read_direct_fn(slot: usize) -> usize {
    varhandle_read_direct_fns()[slot]
}

// ---------------------------------------------------------------------------
// `VarHandle` write-mode thin direct-call helpers.
//
// The callee half of `cratonvm_jit::VARHANDLE_WRITE_DIRECT_FNS`: a
// `(vm_ptr, varhandle, receiver, value)` `extern "C"` function per
// (write mode, value kind) slot. Four arguments, which is exactly Windows'
// ARG_REGS — see the jit-crate header for why `compareAndSet` (five) is not
// bound here.
//
// Unlike the read helpers this arm serves REFERENCE values as well as
// primitives, because the reference travels inward in a register the compiled
// caller's frame already describes; there is no return value to root.
// ---------------------------------------------------------------------------

/// Served / declined counts for the `VarHandle` write direct helpers.
///
/// Same contract as the read pair beside it: `hits == 0` with a bound site is
/// the specific failure this exists to name, and a non-zero `declines` beside a
/// non-zero `hits` is normal — the first call on a handle resolves its field
/// slot inside the native, so it declines and the second qualifies.
///
/// Counted per thread (r9-vmside) — see [`JitCounterBlock::vh_direct_served`].
///
/// `(served, declined)` counts for the `VarHandle` write direct helpers.
pub fn varhandle_write_direct_counts() -> (u64, u64) {
    vh_direct_counts(VH_DIRECT_WRITE)
}

/// Decode one JIT-ABI argument word into the `Value` a field of `kind` holds.
///
/// The inverse of `varhandle_instance_field_read_bits`'s encode, and the same
/// table: an int-category kind is the low half sign-extended by its own width,
/// a `float`/`double` is its bit pattern, and a reference is the raw pointer
/// with zero for null.
///
/// Returns `None` for a word that cannot be a reference — which sends the call
/// to the funnel rather than fabricating an object out of an arbitrary integer.
// SAFETY: Converts raw bit patterns to `Value` according to bytecode descriptor kind; references are checked.
pub unsafe fn varhandle_write_value_of(vm: &SharedVm, kind: u8, bits: i64) -> Option<Value> {
    Some(match kind {
        b'Z' => Value::Int((bits & 1) as i32), // Cast: JIT ABI -- boolean is 0/1
        b'B' => Value::Int(bits as i8 as i32), // Cast: JIT ABI -- byte is sign-extended
        b'C' => Value::Int(bits as u16 as i32), // Cast: JIT ABI -- char is zero-extended
        b'S' => Value::Int(bits as i16 as i32), // Cast: JIT ABI -- short is sign-extended
        b'I' => Value::Int(bits as i32),       // Cast: JIT ABI -- int is the low half
        b'J' => Value::Long(bits),
        b'F' => Value::Float(f32::from_bits(bits as u32)), // Cast: JIT ABI -- float bits
        b'D' => Value::Double(f64::from_bits(bits as u64)), // Cast: JIT ABI -- double bits
        b'L' => {
            if bits == 0 {
                Value::Object(None)
            } else {
                // A non-null word must actually be an object this heap owns.
                // Anything else is not a reference the store may take.
                Value::Object(Some(vm.mem.heap.is_object_address(bits as usize)?))
            }
        }
        _ => return None,
    })
}

/// Body of every [`VARHANDLE_WRITE_DIRECT_FNS`] slot.
///
/// Fast path: the handle describes a resolved instance field whose kind agrees
/// with the call site's value, and the answer is one barriered field store.
/// Everything else — a null handle or coordinate, an unresolved or
/// non-instance-field handle, a kind disagreement, a value word that is not a
/// reference — falls through to the generic dispatcher with this slot's
/// synthetic call site, so the exception and the `VarHandle` access-mode rules
/// stay the registered native's rather than a copy of them.
///
/// The store goes through `set_field_as` (plain `set`) or
/// `set_field_volatile_as` (the other three modes), both of which route to the
/// collector's own inherent write barrier. That matters more here than on the
/// read side: a reference store has to be visible to a concurrent marker, and
/// borrowing the collector's path rather than open-coding one is what keeps
/// this arm and the interpreter's `putfield` telling the GC the same story.
///
/// # The return word is the exception channel
///
/// `0` when served; otherwise whatever the dispatcher returned, which is
/// `i64::MIN` exactly when the native threw. Every door that binds this slot
/// (single-pass, OSR) emits the `JitDirectCall` sequence, and that sequence's
/// `emit_post_invoke_exception_check` compares RAX against `i64::MIN` even
/// for a `V` site. This used to return `()`, so the dispatcher's sentinel was
/// dropped and the compare read whatever the epilogue left in RAX: a
/// `ClassCastException` / `UnsupportedOperationException` the native raised
/// stayed pending while the compiled caller carried on past its own `catch`,
/// to be drained later at an unrelated bci — uncaught in one arm, caught
/// after `bad++` had run in another
/// (r11w7-orch-jit-varhandle-native-exception-escapes-the-local-handler).
///
// SAFETY: Called only from JIT-compiled code, with a `vm_ptr` compiled against this
// live VM and raw references the compiled frame is holding.
pub unsafe fn varhandle_write_direct_impl(
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
    value: i64,
    slot: usize,
    site: *const JitInvokeInfo,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    // Round 15 wave 4 (lane vh6): see `note_thin_varhandle_helper_unjudged`.
    if site.is_null() && crate::runtime::env_cache::dbg_mh_stack() {
        note_thin_varhandle_helper_unjudged(vm, vh, "write", slot);
    }
    // `R` (round 11 wave 18) is a reference slot whose synthetic call site
    // names a non-`Object` class; the store body knows references as `L`.
    let site_kind = match cratonvm_jit::VARHANDLE_WRITE_KINDS
        [slot % cratonvm_jit::VARHANDLE_WRITE_KINDS.len()]
    {
        b'R' => b'L',
        k => k,
    };
    // Mode 0 of `VARHANDLE_WRITE_MODES` is plain `set`.
    //
    // JIT r23 wave 6, lane L2 (`CRATONVM_JIT_VH_THIN_RELEASE_PLAIN`, default
    // ON, x86-64 only): `setRelease` (2) and `setOpaque` (3) take the plain
    // barriered store too. Both tiers' inline constant-handle prefixes already
    // emit a plain `MOV` for exactly these modes (`ir_lower`'s `VhConstOp::Set
    // { volatile: false }`), and under x86-64 TSO a plain store IS a release
    // store; the volatile route's stripe lock and two `SeqCst` fences were
    // ~10 % of `CompletableFuture(Object)`'s `RESULT.setRelease` on this
    // helper (n23-w5 RIP sample). `setVolatile` (1) keeps the volatile route.
    let mode = slot / cratonvm_jit::VARHANDLE_WRITE_KINDS.len();
    let plain = mode == 0
        || (matches!(mode, 2 | 3)
            && cfg!(target_arch = "x86_64")
            && crate::runtime::interpreter::jit_bridge::vh_thin_release_plain_enabled(vm));
    // Round 15 wave 6 (lane vh8): see `varhandle_thin_fast_arm_admitted`.
    if vh != 0 && receiver != 0 && varhandle_thin_fast_arm_admitted(vm, vh, site) {
        if varhandle_instance_field_write(vm, vh as u64, receiver as u64, site_kind, value, plain) {
            note_vh_direct(VH_DIRECT_WRITE, true);
            note_funnel_bypass_hit();
            return 0;
        }
    }
    note_vh_direct(VH_DIRECT_WRITE, false);
    let args = [vh, receiver, value];
    jit_invoke_dispatch(
        vm_ptr,
        varhandle_thin_cold_arm_info(site, &VARHANDLE_WRITE_INFOS[slot]),
        args.as_ptr() as i64,
        3,
    )
}

/// nio21 wave 4 L4: the SATB pre-barrier a THIN helper owes before it stores a
/// reference field through the heap API (`set_field{,_as,_volatile_as}`) -- the
/// same sequence `NativeContextImpl`'s field stores run
/// (`vm_exec::publish_overwritten_field`, round 16 wave 9): on the Generational
/// heap while its SATB queue is active, read the old value through the store's
/// own descriptor and hand a non-null one to `satb_barrier`. G1 and ZGC publish
/// inside their accessors, so this does nothing there (publishing again would
/// only log twice). Call it with the FORWARDED receiver, before the store.
/// Returns whether a reference was published (for tests).
///
/// nio21 wave 5 L1 (W4-L4-1): no longer a copy -- the one definition is
/// `vm_exec::publish_overwritten_field` (now `pub`), with the store's
/// descriptor and the native store's kill switch (`CRATONVM_NATIVE_STORE_SATB`),
/// so the decision (marking Generational heap, a slot that may hold a
/// reference, the switch asked last) cannot drift from the native's.
#[inline]
pub fn thin_store_publish_overwritten_field(
    heap: &VmHeap,
    obj: ObjectRef,
    index: usize,
    desc: u8,
) -> bool {
    crate::vm::vm_exec::publish_overwritten_field(
        heap,
        crate::runtime::env_cache::native_store_satb,
        obj,
        index,
        Some(desc),
    )
}


/// The store itself, shared by the direct arm and the funnel-side twin so the
/// two cannot drift apart in what they consider a servable handle.
///
/// `true` when the write happened; `false` declines to the caller's cold arm.
///
/// # SAFETY
///
/// `vh_raw` and `recv_raw` are raw references a compiled frame is holding.
pub unsafe fn varhandle_instance_field_write(
    vm: &SharedVm,
    vh_raw: u64,
    recv_raw: u64,
    site_kind: u8,
    value_bits: i64,
    plain: bool,
) -> bool {
    // The per-GC-epoch receiver memo, exactly as the read side uses it
    // (`varhandle_instance_field_read_bits`): the two `is_object_address`
    // walks plus `java_identity_hash` per call it replaces were 10.3% of a
    // read profile. Its `identity` is the GC-stable key `vh_meta_get` files
    // the handle under.
    let Some((vh_class_id, key)) = direct_receiver_facts(vm, vh_raw as usize) else {
        return false;
    };
    if vh_class_id == 0 {
        return false;
    }
    let Some((recv_class_id, _)) = direct_receiver_facts(vm, recv_raw as usize) else {
        return false;
    };
    if recv_class_id == 0 {
        return false;
    }
    let receiver = ObjectRef::from_raw(recv_raw as usize as *mut u8);
    let heap = &vm.mem.heap;
    // This handle's own plan (gc-common w36-a), as on the read side.
    let Some(plan) =
        cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle(
            vm.vm_identity,
            key,
            vh_raw as usize,
        )
    else {
        return false;
    };
    // The three refusals the native makes before it stores (round 11 wave 3):
    // a handle over a `final` field (UnsupportedOperationException, and the
    // native owns the kill switch), a receiver that is not an instance of the
    // handle's class, and — below, once the value is decoded — a reference
    // the field's declared type does not admit (both ClassCastException).
    // Serving any of them here wrote the final field / the wrong object.
    if plan.read_only || !varhandle_plan_admits_receiver(vm, &plan, recv_class_id) {
        return false;
    }
    // Reference/primitive agreement between the variable and the call site,
    // the same test the read side makes.
    let site_is_ref = site_kind == b'L';
    let plan_is_ref = plan.value_desc == b'L';
    if site_is_ref != plan_is_ref || (!site_is_ref && site_kind != plan.value_desc) {
        return false;
    }
    let Some(value) = varhandle_write_value_of(vm, plan.value_desc, value_bits) else {
        return false;
    };
    if !varhandle_plan_admits_value(vm, &plan, &value) {
        return false;
    }
    let receiver = heap.load_and_forward(receiver);
    crate::runtime::frozen_pages::note_slot_write(vm, receiver, plan.field_index as usize, true);
    // nio21 wave 4 L4 (page
    // `r21-varhandle-thin-write-skips-the-generational-native-store-satb-FIXED-20261008.md`):
    // the SATB pre-barrier of the NATIVE store this stands in front of. The
    // Generational accessors below card-mark only; G1 and ZGC publish inside
    // them. Without this a compiled `VarHandle.set*` during an old-generation
    // concurrent mark hid the overwritten referent from the marker.
    thin_store_publish_overwritten_field(
        heap,
        receiver,
        plan.field_index as usize,
        plan.value_desc,
    );
    // Plain `set` is a plain store, exactly what the interpreter's `putfield`
    // and the registered `varhandle_set` native do (round 11: it paid the
    // volatile stripe lock and two SeqCst fences per call). The other three
    // bound modes stay volatile: the STRONGEST, and always a legal
    // implementation of `setRelease`/`setOpaque`. Both routes fire the
    // collector's inherent write barrier inside `set_field`.
    if plain {
        heap.set_field_as(receiver, plan.field_index as usize, value, plan.value_desc);
    } else {
        heap.set_field_volatile_as(receiver, plan.field_index as usize, value, plan.value_desc);
    }
    true
}

/// The thin direct-call target for slot `SLOT` of
/// `cratonvm_jit::VARHANDLE_WRITE_DIRECT_FNS`.
///
/// One monomorphisation per slot, so the slot — and with it the site's value
/// kind and the synthetic call site the cold arm uses — is a compile-time
/// constant in the emitted `CALL`'s target.
///
/// Returns `0`, or `i64::MIN` when an exception is pending — see
/// [`varhandle_write_direct_impl`]. The panic sentinel is `i64::MIN` for the
/// same reason: the stashed `InternalError` is then delivered at this call's
/// bci instead of at some later drain.
///
/// # SAFETY
///
/// See [`varhandle_write_direct_impl`].
pub unsafe extern "C" fn jit_varhandle_write_direct<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
    value: i64,
) -> i64 {
    // Round 15 wave 6 (lane vh8): a judged bind's first word is its site.
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let f = || varhandle_write_direct_impl(vm_ptr, vh, receiver, value, SLOT, site);
    contain(
        "jit_varhandle_write_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Addresses of every [`jit_varhandle_write_direct`] monomorphisation, in slot
/// order.
pub fn varhandle_write_direct_fns() -> [usize; cratonvm_jit::VARHANDLE_WRITE_SLOTS] {
    macro_rules! slots {
        ($($slot:literal),* $(,)?) => {
            [$( jit_varhandle_write_direct::<$slot> as *const () as usize ),*]
        };
    }
    slots!(
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39,
    )
}

/// The thin direct-call helper address for one write slot, for the OSR door.
pub fn varhandle_write_direct_fn(slot: usize) -> usize {
    varhandle_write_direct_fns()[slot]
}

/// Erased call-site descriptors for [`VARHANDLE_WRITE_INFOS`], indexed by the
/// value-kind half of a slot.
///
/// A baked direct call has no `JitInvokeInfo` of its own, so the cold arm
/// cannot hand the generic dispatcher the site's real descriptor. These stand
/// in for it. The substitution is not observable here for the same reason it is
/// not on the read side for a primitive: the descriptor's readers are the
/// argument decode (one reference coordinate and one value either way) and the
/// return handling, and a `void` return has none. The REFERENCE value slot is
/// safe for the same reason the read side's reference RETURN was not —
/// `varhandle_reference_return_mismatch` compares a returned object against the
/// site's declared class, and there is no returned object here.
///
/// There is ONE reader for which the value's static type IS observable: the
/// site-cached dispatch arms the native's value-site cell from this descriptor
/// (`poly_call_site::vh_value_site_classes`), and for a PRIMITIVE variable the
/// native decides CCE vs `WrongMethodTypeException` by it. So the reference
/// kind is split by static-type group (`cratonvm_jit`'s
/// `varhandle_reference_value_kind`): the `L` slot's `Object` stand-in is exact
/// for an `Object`-equivalent site, `Number`/wrapper-typed sites are not bound
/// at all, and every other class and every array takes the `R` slot, whose
/// stand-in `java/lang/Void` the classifier files as `OTHER` — so a primitive
/// variable refuses it statically (WMTE, value or null), as HotSpot does for
/// a `String`- or `Node`-typed site (round 11 wave 18,
/// `r11w17-vh-varhandle-reference-slot-group-patch-FIXED-20260926.md`). On a
/// REFERENCE variable the native does not consult the site class for a
/// reference value, so `R` answers exactly as `L` there. The argument decode
/// reads only the `L` tag of either.
pub const VARHANDLE_WRITE_DESCRIPTORS: [&str; 10] = [
    "(Ljava/lang/Object;Z)V",
    "(Ljava/lang/Object;B)V",
    "(Ljava/lang/Object;C)V",
    "(Ljava/lang/Object;S)V",
    "(Ljava/lang/Object;I)V",
    "(Ljava/lang/Object;J)V",
    "(Ljava/lang/Object;F)V",
    "(Ljava/lang/Object;D)V",
    "(Ljava/lang/Object;Ljava/lang/Object;)V",
    "(Ljava/lang/Object;Ljava/lang/Void;)V",
];

/// Synthetic call sites for the cold arm of the `VarHandle` write helpers, one
/// per slot. `static` for the same reason [`VARHANDLE_READ_INFOS`] is:
/// `jit_invoke_dispatch` keys its per-site memos on `(vm_identity, info
/// address)`, so the address has to be process-stable.
pub static VARHANDLE_WRITE_INFOS: [JitInvokeInfo; cratonvm_jit::VARHANDLE_WRITE_SLOTS] = [
    vh_write_info(0, 0),
    vh_write_info(0, 1),
    vh_write_info(0, 2),
    vh_write_info(0, 3),
    vh_write_info(0, 4),
    vh_write_info(0, 5),
    vh_write_info(0, 6),
    vh_write_info(0, 7),
    vh_write_info(0, 8),
    vh_write_info(0, 9),
    vh_write_info(1, 0),
    vh_write_info(1, 1),
    vh_write_info(1, 2),
    vh_write_info(1, 3),
    vh_write_info(1, 4),
    vh_write_info(1, 5),
    vh_write_info(1, 6),
    vh_write_info(1, 7),
    vh_write_info(1, 8),
    vh_write_info(1, 9),
    vh_write_info(2, 0),
    vh_write_info(2, 1),
    vh_write_info(2, 2),
    vh_write_info(2, 3),
    vh_write_info(2, 4),
    vh_write_info(2, 5),
    vh_write_info(2, 6),
    vh_write_info(2, 7),
    vh_write_info(2, 8),
    vh_write_info(2, 9),
    vh_write_info(3, 0),
    vh_write_info(3, 1),
    vh_write_info(3, 2),
    vh_write_info(3, 3),
    vh_write_info(3, 4),
    vh_write_info(3, 5),
    vh_write_info(3, 6),
    vh_write_info(3, 7),
    vh_write_info(3, 8),
    vh_write_info(3, 9),
];

/// One entry of [`VARHANDLE_WRITE_INFOS`]. `num_jit_args: 3` counts the
/// receiver — the `VarHandle` itself — plus the coordinate and the value.
pub const fn vh_write_info(mode: usize, kind: usize) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/VarHandle",
        method_name: cratonvm_jit::VARHANDLE_WRITE_MODES[mode],
        descriptor: VARHANDLE_WRITE_DESCRIPTORS[kind],
        num_jit_args: 3,
        return_type: b'V',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

// ---------------------------------------------------------------------------
// `VarHandle` compareAndSet thin direct-call helpers.
//
// The callee half of `cratonvm_jit::VARHANDLE_CAS_DIRECT_FNS`: a
// `(vm_ptr, varhandle, receiver, expected, new)` `extern "C"` function per
// value kind. Five arguments — five of SysV's six `ARG_REGS`, and on Win64 the
// fifth arrives on the stack, which `emit_stack_arg_setup` marshals.
// ---------------------------------------------------------------------------

/// Served / declined counts for the `VarHandle` CAS direct helpers.
///
/// Same contract as the read and write pairs beside them: `hits == 0` with a
/// bound site is the specific failure this exists to name, and a non-zero
/// `declines` beside a non-zero `hits` is normal — the first call on a handle
/// resolves its field slot inside the native, so it declines and the second
/// qualifies.
///
/// Counted per thread (r9-vmside) — see [`JitCounterBlock::vh_direct_served`].
///
/// `(served, declined)` counts for the `VarHandle` CAS direct helpers.
pub fn varhandle_cas_direct_counts() -> (u64, u64) {
    vh_direct_counts(VH_DIRECT_CAS)
}

/// The CAS itself, shared by the direct arm and the funnel-side twin so the two
/// cannot drift apart in what they consider a servable handle.
///
/// `Some(swapped)` when the CAS happened; `None` declines to the caller's cold
/// arm. The barrier ordering is not here — it is one level further down in
/// `compare_and_swap_field_shared`, which both the interpreter and this share,
/// because a CAS has more to get wrong than a store: the SATB pre-barrier fires
/// on `expected` BEFORE the store and the post `write_barrier` only on success.
///
/// # SAFETY
///
/// `vh_raw` and `recv_raw` are raw references a compiled frame is holding.
pub unsafe fn varhandle_instance_field_cas_shared(
    vm: &SharedVm,
    vh_raw: u64,
    recv_raw: u64,
    site_kind: u8,
    expected_bits: i64,
    new_bits: i64,
) -> Option<bool> {
    let (receiver, field_index, expected, new_val) = varhandle_instance_field_cas_operands(
        vm,
        vh_raw,
        recv_raw,
        site_kind,
        expected_bits,
        new_bits,
    )?;
    Some(crate::vm::vm_exec::compare_and_swap_field_shared(
        vm,
        receiver,
        field_index,
        expected,
        new_val,
    ))
}

/// The checks [`varhandle_instance_field_cas_shared`] makes before its CAS,
/// shared with the interpreter's `compareAndExchange` arm
/// ([`varhandle_instance_field_cae_values`], interpreter round i2 wave 54,
/// lane L7) so the two arms decline exactly the same accesses:
/// `(forwarded receiver, field slot, expected, new)` of a servable access,
/// `None` for anything the registered native has to judge.
///
/// # SAFETY
///
/// As [`varhandle_instance_field_cas_shared`].
pub unsafe fn varhandle_instance_field_cas_operands(
    vm: &SharedVm,
    vh_raw: u64,
    recv_raw: u64,
    site_kind: u8,
    expected_bits: i64,
    new_bits: i64,
) -> Option<(ObjectRef, usize, Value, Value)> {
    // The per-GC-epoch receiver memo, as the read and write sides use it; its
    // `identity` is the GC-stable key `vh_meta_get` files the handle under.
    let (vh_class_id, key) = direct_receiver_facts(vm, vh_raw as usize)?;
    if vh_class_id == 0 {
        return None;
    }
    let (recv_class_id, _) = direct_receiver_facts(vm, recv_raw as usize)?;
    if recv_class_id == 0 {
        return None;
    }
    let receiver = ObjectRef::from_raw(recv_raw as usize as *mut u8);
    let heap = &vm.mem.heap;
    // This handle's own plan (gc-common w36-a), as on the read side.
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle(
        vm.vm_identity,
        key,
        vh_raw as usize,
    )?;
    // The native's refusals, as on the write side: a `final` field's handle,
    // a receiver of the wrong class, and (below) a NEW value the field's type
    // does not admit. The expected value is only compared, never stored, and
    // the native does not judge it either.
    if plan.read_only || !varhandle_plan_admits_receiver(vm, &plan, recv_class_id) {
        return None;
    }
    // Reference/primitive agreement between the variable and the call site, the
    // same test the read and write sides make. `site_kind` is the bound slot's
    // constant here and the parsed descriptor's answer on the funnel side; both
    // are statements about what the CALLER passed, which the erased registered
    // descriptor cannot make.
    let site_is_ref = site_kind == b'L';
    let plan_is_ref = plan.value_desc == b'L';
    if site_is_ref != plan_is_ref || (!site_is_ref && site_kind != plan.value_desc) {
        return None;
    }
    let expected = varhandle_operand_value(vm, expected_bits, plan.value_desc)?;
    let new_val = varhandle_operand_value(vm, new_bits, plan.value_desc)?;
    if !varhandle_plan_admits_value(vm, &plan, &new_val) {
        return None;
    }
    // nio21 wave 11 L5: HotSpot 25 casts the EXPECTED value to the field type
    // as well (`FieldInstanceReadWrite.compareAndSet` /
    // `compareAndExchange*`: `handle.fieldType.cast(expected)`), so a
    // wrong-typed expectation is a `ClassCastException`, not a failed compare
    // (`Nio21VarHandleConst.java` row `exc wrongExpected`). This
    // arm cannot raise it; it declines to the registered native, which owns
    // that answer (`r21-varhandle-instance-cas-does-not-cast-expected-FIXED-20261009.md`).
    // `CRATONVM_JIT_VH_CAS_EXPECTED_SCREEN=0` restores the old acceptance.
    // The switch is read only on a refusal, off the common path.
    if !varhandle_plan_admits_value(vm, &plan, &expected)
    {
        return None;
    }
    let receiver = heap.load_and_forward(receiver);
    crate::runtime::frozen_pages::note_slot_write(vm, receiver, plan.field_index as usize, true);
    Some((receiver, plan.field_index as usize, expected, new_val)) // Cast: field slot index
}

/// The `compareAndSet` operand kind of an interpreter call site (`descriptor`,
/// whose return the caller has checked is `Z`): the common kind of `expected`
/// and `new` when the site is exactly `(receiver, expected, new)` with both
/// of one kind ([`varhandle_cas_operand_kinds`], references collapsed to `L`),
/// else `None`. Read once per site by the interpreter's signature-polymorphic
/// site cache (`sig_poly_site`, JIT round 16 wave 7 lane varhandle, MH16-6).
pub fn varhandle_cas_site_kind(descriptor: &str) -> Option<u8> {
    match varhandle_cas_operand_kinds(descriptor)? {
        (expected, new) if expected == new => Some(expected),
        _ => None,
    }
}

/// [`varhandle_instance_field_cas_shared`] over the interpreter's popped
/// arguments (MH16-6 of `jit-r16-mh-proposals-RETIRED-20261006.md`, JIT round 16 wave 7, lane
/// varhandle; `CRATONVM_VH_INTERP_CAS_FAST`, default ON since wave 7, `=0` kills it). `site_kind` is
/// [`varhandle_cas_site_kind`]'s answer for the site. Each operand is encoded
/// as the JIT ABI word the shared body decodes; a `Value` whose variant does
/// not match `site_kind` declines. `None` declines to the registered native,
/// which then judges the access exactly as without this arm.
///
/// Nothing here allocates, runs Java or reaches a safepoint, so the
/// references the interpreter popped stay valid for the call (the funnel twin
/// holds raw compiled-frame words under the same rule).
pub fn varhandle_instance_field_cas_values(
    vm: &SharedVm,
    vh: ObjectRef,
    receiver: ObjectRef,
    site_kind: u8,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    let expected = vh_interp_operand_word(expected, site_kind)?;
    let new_val = vh_interp_operand_word(new_val, site_kind)?;
    // SAFETY: both references were just popped from a live interpreter frame;
    // the shared body validates each against the heap (memoised per GC epoch)
    // before reading it, and nothing between the pop and the CAS can move it.
    unsafe {
        varhandle_instance_field_cas_shared(
            vm,
            vh.as_ptr() as usize as u64,       // Cast: raw reference word
            receiver.as_ptr() as usize as u64, // Cast: raw reference word
            site_kind,
            expected,
            new_val,
        )
    }
}

/// An interpreter operand as the JIT ABI word the shared CAS body decodes; a
/// `Value` whose variant does not match `site_kind` is `None`.
pub fn vh_interp_operand_word(value: Value, site_kind: u8) -> Option<i64> {
    Some(match (site_kind, value) {
        (b'L', Value::Object(None)) => 0,
        (b'L', Value::Object(Some(r))) => r.as_ptr() as usize as i64, // Cast: JIT ABI -- reference word
        (b'Z' | b'B' | b'C' | b'S' | b'I', Value::Int(i)) => i64::from(i),
        (b'J', Value::Long(l)) => l,
        (b'F', Value::Float(f)) => i64::from(f.to_bits()),
        (b'D', Value::Double(d)) => d.to_bits() as i64, // Cast: JIT ABI -- double bits
        _ => return None,
    })
}

/// The `compareAndExchange` (plain, `Acquire`, `Release`) operand kind of an
/// interpreter call site the route cache's exchange arm serves (interpreter
/// round i2 wave 54, lane L7; `CRATONVM_VH_INTERP_CAE_FAST`): exactly
/// `(receiver, expected, new)` with both values `int` and an `int` return, or
/// all three `long`. Every other shape (a reference, a sub-word or floating
/// variable) stays the registered native's: those either need a load barrier
/// on the witness or have no hardware cell ([`compare_and_exchange_field_hw`]).
pub fn varhandle_cae_site_kind(descriptor: &str, ret_tag: u8) -> Option<u8> {
    match varhandle_cas_operand_kinds(descriptor)? {
        (expected, new) if expected == new && new == ret_tag && matches!(ret_tag, b'I' | b'J') => {
            Some(ret_tag)
        }
        _ => None,
    }
}

/// `VarHandle.compareAndExchange` on an instance field for the interpreter's
/// route cache (interpreter round i2 wave 54, lane L7, proposal
/// `i53-L7-proposal-an-instance-field-arm-for-compare-and-exchange`;
/// `VmConfig::vh_interp_cae_fast`): the witness, or `None` to decline to the
/// registered native. The same checks as the `compareAndSet` arm
/// ([`varhandle_instance_field_cas_operands`]), so the two decline the same
/// accesses; then one hardware compare-exchange on the field's `int` / `long`
/// cell ([`compare_and_exchange_field_hw`]), whose failure value is the
/// witness. A field the hardware path cannot address declines (nothing was
/// written). `site_kind` is [`varhandle_cae_site_kind`]'s answer.
///
/// Nothing here allocates, runs Java or reaches a safepoint (as
/// [`varhandle_instance_field_cas_values`]).
pub fn varhandle_instance_field_cae_values(
    vm: &SharedVm,
    vh: ObjectRef,
    receiver: ObjectRef,
    site_kind: u8,
    expected: Value,
    new_val: Value,
) -> Option<Value> {
    let expected = vh_interp_operand_word(expected, site_kind)?;
    let new_val = vh_interp_operand_word(new_val, site_kind)?;
    // SAFETY: as `varhandle_instance_field_cas_values`.
    let (receiver, field_index, expected, new_val) = unsafe {
        varhandle_instance_field_cas_operands(
            vm,
            vh.as_ptr() as usize as u64,       // Cast: raw reference word
            receiver.as_ptr() as usize as u64, // Cast: raw reference word
            site_kind,
            expected,
            new_val,
        )?
    };
    compare_and_exchange_field_hw(vm, receiver, field_index, expected, new_val)
}

/// The hardware compare-exchange of an `int` / `long` instance field: the
/// value the cell held (the witness: `expected` on success), or `None` when
/// the slot is not a hardware cell of that width (`hw_atomic_addr`, the same
/// test `hw_cas_primitive_field` makes, so a compiled and an interpreted CAS
/// address the same word). No barrier: the payload is primitive.
pub fn compare_and_exchange_field_hw(
    vm: &SharedVm,
    receiver: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<Value> {
    let fwd = vm.mem.heap.load_and_forward(receiver);
    crate::vm::vm_exec::hw_cae_primitive_field(vm, fwd, index, expected, new_val)
}

/// Body of every [`VARHANDLE_CAS_DIRECT_FNS`] slot.
///
/// Fast path: the handle describes a resolved instance field whose kind agrees
/// with the call site's operands, and the answer is one barriered CAS.
/// Everything else — a null handle or coordinate, an unresolved or
/// non-instance-field handle, a kind disagreement, an operand word that is not
/// a reference — falls through to the generic dispatcher with this slot's
/// synthetic call site, so the exception and the `VarHandle` access-mode rules
/// stay the registered native's rather than a copy of them.
///
/// # SAFETY
///
/// Called only from JIT-compiled code, with a `vm_ptr` compiled against this
/// live VM and raw references the compiled frame is holding.
pub unsafe fn varhandle_cas_direct_impl(
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
    expected: i64,
    new_val: i64,
    slot: usize,
    site: *const JitInvokeInfo,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    // Round 15 wave 4 (lane vh6): see `note_thin_varhandle_helper_unjudged`.
    if site.is_null() && crate::runtime::env_cache::dbg_mh_stack() {
        note_thin_varhandle_helper_unjudged(vm, vh, "compareAndSet", slot);
    }
    // `R` (round 11 wave 18) is a reference slot whose synthetic call site
    // names a non-`Object` class; the CAS body knows references as `L`.
    let site_kind =
        match cratonvm_jit::VARHANDLE_CAS_KINDS[slot % cratonvm_jit::VARHANDLE_CAS_KINDS.len()] {
            b'R' => b'L',
            k => k,
        };
    // Round 15 wave 6 (lane vh8): see `varhandle_thin_fast_arm_admitted`.
    if vh != 0 && receiver != 0 && varhandle_thin_fast_arm_admitted(vm, vh, site) {
        if let Some(swapped) = varhandle_instance_field_cas_shared(
            vm,
            vh as u64,
            receiver as u64,
            site_kind,
            expected,
            new_val,
        ) {
            note_vh_direct(VH_DIRECT_CAS, true);
            note_funnel_bypass_hit();
            return i64::from(swapped);
        }
    }
    note_vh_direct(VH_DIRECT_CAS, false);
    let args = [vh, receiver, expected, new_val];
    jit_invoke_dispatch(
        vm_ptr,
        varhandle_thin_cold_arm_info(site, &VARHANDLE_CAS_INFOS[slot]),
        args.as_ptr() as i64,
        4,
    )
}

/// The thin direct-call target for slot `SLOT` of
/// `cratonvm_jit::VARHANDLE_CAS_DIRECT_FNS`.
///
/// One monomorphisation per slot, so the slot — and with it the site's value
/// kind and the synthetic call site the cold arm uses — is a compile-time
/// constant in the emitted `CALL`'s target.
///
/// # SAFETY
///
/// See [`varhandle_cas_direct_impl`].
pub unsafe extern "C" fn jit_varhandle_cas_direct<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    receiver: i64,
    expected: i64,
    new_val: i64,
) -> i64 {
    // Round 15 wave 6 (lane vh8): a judged bind's first word is its site.
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let f = || varhandle_cas_direct_impl(vm_ptr, vh, receiver, expected, new_val, SLOT, site);
    contain(
        "jit_varhandle_cas_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Addresses of every [`jit_varhandle_cas_direct`] monomorphisation, in slot
/// order.
pub fn varhandle_cas_direct_fns() -> [usize; cratonvm_jit::VARHANDLE_CAS_SLOTS] {
    macro_rules! slots {
        ($($slot:literal),* $(,)?) => {
            [$( jit_varhandle_cas_direct::<$slot> as *const () as usize ),*]
        };
    }
    slots!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9)
}

/// The thin direct-call helper address for one CAS slot, for the OSR door.
pub fn varhandle_cas_direct_fn(slot: usize) -> usize {
    varhandle_cas_direct_fns()[slot]
}

/// Erased call-site descriptors for [`VARHANDLE_CAS_INFOS`], indexed by slot.
///
/// A baked direct call has no `JitInvokeInfo` of its own, so the cold arm
/// cannot hand the generic dispatcher the site's real descriptor. These stand
/// in for it, and the substitution is unobservable for the same reason it is on
/// the write side: the descriptor's readers are the argument decode — one
/// reference coordinate and two values of this kind either way — and the return
/// handling, which is a `boolean` in every slot. The operands' STATIC types
/// are observable through the value-site cell, as on the write side (see
/// [`VARHANDLE_WRITE_DESCRIPTORS`] for the `L`/`R` split). The `R` slot's
/// stand-in names `java/lang/Void` for BOTH operands: `asType` checks every
/// parameter before converting any, so a site with one non-`Object`-group
/// operand is a WMTE on a primitive variable whatever the other operand is.
/// `try_varhandle_instance_field_cas`'s parser collapses `Void` to `L`.
pub const VARHANDLE_CAS_DESCRIPTORS: [&str; 10] = [
    "(Ljava/lang/Object;ZZ)Z",
    "(Ljava/lang/Object;BB)Z",
    "(Ljava/lang/Object;CC)Z",
    "(Ljava/lang/Object;SS)Z",
    "(Ljava/lang/Object;II)Z",
    "(Ljava/lang/Object;JJ)Z",
    "(Ljava/lang/Object;FF)Z",
    "(Ljava/lang/Object;DD)Z",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    "(Ljava/lang/Object;Ljava/lang/Void;Ljava/lang/Void;)Z",
];

/// Decode a thin `VarHandle` helper's first argument word (round 15 wave 6,
/// lane vh8; VH7-1 of `jit-r15w5-vh7-proposals-RETIRED-20261003.md`).
///
/// An UNJUDGED bind passes the `SharedVm` pointer, as every bind did before;
/// a JUDGED bind (`cratonvm_jit::JIT_VARHANDLE_JUDGED_GUARD`, built by
/// `cratonvm_jit::VarHandleJudgedSite`) passes its site record's address with
/// `cratonvm_jit::VARHANDLE_JUDGED_SITE_TAG` set. Both addresses are
/// 8-aligned, so the bit is the whole test. Returns the `SharedVm` pointer and
/// the site's own `JitInvokeInfo`, or null for an unjudged bind (whose cold arm
/// keeps the slot's stand-in).
///
/// # SAFETY
///
/// `word` is the first argument compiled code passed one of the three thin
/// helper families: a `SharedVm` pointer, or the tagged address of a record
/// owned by the very body making the call (so alive for the call).
#[inline(always)]
pub unsafe fn varhandle_thin_call_site(word: i64) -> (i64, *const JitInvokeInfo) {
    // Cast: the JIT ABI word is an address.
    let w = word as usize;
    if w & cratonvm_jit::VARHANDLE_JUDGED_SITE_TAG == 0 {
        return (word, std::ptr::null());
    }
    // SAFETY: the tag is set only by a judged row, whose record the calling
    // body owns (`CompiledMethod::_jit_vh_judged_sites`).
    let site = &*((w & !cratonvm_jit::VARHANDLE_JUDGED_SITE_TAG)
        as *const cratonvm_jit::VarHandleJudgedSite);
    // Cast: the record's `SharedVm` address back to the JIT ABI word.
    (site.vm_ptr as i64, &site.info as *const JitInvokeInfo)
}

/// May a thin `VarHandle` helper serve this call on its FAST arm (round 15
/// wave 6, lane vh8; VH7-1)?
///
/// The fast arm cannot judge an invoke-exact handle: it never sees a
/// descriptor. An unjudged bind (`site` null) answers yes, as it always has;
/// the first-mint sweep is what keeps such binds away from exact handles. A
/// JUDGED bind answers no for an exact receiver once this VM has minted one,
/// so the access takes the cold arm, whose `jit_invoke_dispatch` runs with the
/// site's REAL `JitInvokeInfo` and judges it (the site-cached dispatch, the
/// generic tail) exactly as at an unbound site: a mismatching descriptor is a
/// `WrongMethodTypeException` at the call's bci, a matching one is served.
/// Before the VM's first exact mint this is one relaxed load; after it, the
/// fast arm's own memoised receiver validation and one field read.
///
/// # SAFETY
///
/// `vh` is a raw reference from compiled code; it is validated (memoised, per
/// GC epoch) as a heap object before it is read.
#[inline]
pub unsafe fn varhandle_thin_fast_arm_admitted(
    vm: &SharedVm,
    vh: i64,
    site: *const JitInvokeInfo,
) -> bool {
    if site.is_null() {
        return true;
    }
    let Some(exact_slot) = crate::runtime::interpreter::var_handle_exact_gate(vm) else {
        return true;
    };
    if vh == 0 {
        // The fast arms decline a null handle themselves.
        return true;
    }
    match direct_receiver_facts(vm, vh as usize) {
        Some((class_id, _)) if class_id != 0 => {
            !crate::runtime::interpreter::var_handle_receiver_is_exact(
                vm,
                ObjectRef::from_raw(vh as usize as *mut u8),
                exact_slot,
            )
        }
        // Not a live object (or an array): the fast arm declines it anyway.
        _ => true,
    }
}

/// The `JitInvokeInfo` a thin `VarHandle` helper's cold arm dispatches with,
/// as `jit_invoke_dispatch`'s word: the judged site's own (round 15 wave 6,
/// lane vh8), else the slot's stand-in.
#[inline(always)]
pub fn varhandle_thin_cold_arm_info(
    site: *const JitInvokeInfo,
    stand_in: &'static JitInvokeInfo,
) -> i64 {
    let info = if site.is_null() {
        stand_in as *const JitInvokeInfo
    } else {
        site
    };
    // Cast: the JIT ABI word is an address.
    info as i64
}


/// Is `info` one of the thin `VarHandle` helpers' synthetic cold-arm sites
/// ([`VARHANDLE_READ_INFOS`], [`VARHANDLE_WRITE_INFOS`],
/// [`VARHANDLE_CAS_INFOS`])? Their descriptor is a stand-in shared by every
/// bound site of the slot, not the call site's, so nothing that compares the
/// call site's exact type (an invoke-exact `VarHandle`, interpreter round i1
/// wave 37, lane L4) may read it.
pub fn is_varhandle_stand_in_info(info: &JitInvokeInfo) -> bool {
    let p: *const JitInvokeInfo = info;
    VARHANDLE_READ_INFOS.as_ptr_range().contains(&p)
        || VH_BYTE_ARRAY_VIEW_READ_INFOS.as_ptr_range().contains(&p)
        || VARHANDLE_WRITE_INFOS.as_ptr_range().contains(&p)
        || VARHANDLE_CAS_INFOS.as_ptr_range().contains(&p)
}

/// May a compile in `shared` bind the thin `VarHandle` helpers (round 15 wave
/// 2, lane vh2; `r15w1-vh-jit-varhandle-thin-helpers-exact-patch-FIXED-20260930.md`)?
///
/// Not once this VM has minted an invoke-exact handle
/// ([`crate::runtime::interpreter::var_handle_exact_gate`]). A bound site has
/// no `JitInvokeInfo` of its own, so neither arm of a helper can judge an
/// exact receiver against the site's descriptor: the fast arm serves an exact
/// field handle unjudged, and the cold arm dispatches through the slot's
/// stand-in, which [`is_varhandle_stand_in_info`] rightly keeps out of the
/// judgement. An unbound site reaches `jit_invoke_dispatch` with its REAL
/// info, whose site-cached dispatch and generic tail judge it: the road
/// `--real-jdk` and `CRATONVM_JIT_VARHANDLE_*_DIRECT_HELPERS=0` already take.
/// A VM that never minted an exact handle pays one relaxed load per compile
/// and keeps every bind.
///
/// Read by `direct_helper_table_for` (the MethodEntry and optimizing doors)
/// and the OSR door in `jit_bridge.rs`. A body compiled BEFORE the first mint
/// keeps its binds (`r15w2-vh2-thin-helpers-bound-before-first-exact-mint-FIXED-20260930.md`).
/// `CRATONVM_JIT_VH_THIN_BIND_EXACT_GATE=0` binds regardless (the round-15
/// wave-1 behaviour). The switch was read uncached on EVERY compile once the
/// gate opened (three times per `VarHandle` site at the OSR door); round 15
/// wave 3 (lane vh3) reads it through `lang_invoke`'s cached accessor.
pub fn varhandle_thin_binds_admitted(shared: &SharedVm) -> bool {
    crate::runtime::interpreter::var_handle_exact_gate(shared).is_none()
        || !cratonvm_native_builtins::lang_invoke::vh_thin_bind_exact_gate_enabled()
}

/// May the single-pass bind ladder of a compile in `shared` bind a thin
/// `VarHandle` helper (round 15 wave 6, lane vh8; VH7-1)? Always while the
/// ladder binds JUDGED (`cratonvm_jit::varhandle_judged_binds_enabled`): a
/// judged helper refuses a mismatching exact handle itself, so the VM's first
/// exact mint no longer has to withdraw the bind. Otherwise
/// [`varhandle_thin_binds_admitted`]'s gate, as before. Read by
/// `direct_helper_table_for` (the table the ladder binds from), the splice
/// refusal that protects the ladder's binds, and the OSR door's three
/// `VarHandle` arms, which build the same judged rows
/// (`r15w6-vh8-osr-door-judged-binds-patch-FIXED-20260930.md`). Pre-mint: one relaxed
/// load; post-mint, the switch's uncached read, once per compile.
pub fn varhandle_method_entry_binds_admitted(shared: &SharedVm) -> bool {
    varhandle_thin_binds_admitted(shared) || cratonvm_jit::varhandle_judged_binds_enabled()
}

/// May read slot `slot` be bound to its thin helper (round 15 wave 2
/// hand-back, lane vh2; VH2-3,
/// `r15w2-vh2-varhandle-reference-return-not-cast-to-site-type-FIXED-20260930.md`)?
///
/// Not a `REF_STRICT` slot while `VarHandle` reference returns are cast
/// (`lang_invoke::vh_reference_return_checkcast_enabled`): its site declares a
/// class the helper cannot name, so neither arm can cast to it (the fast arm
/// returns the field as it is, the cold arm's stand-in is `(Object)Object`).
/// Unbound, the site reaches the site-cached dispatch, whose fast path and
/// native road both cast. `REF_OBJECT` and the primitive slots need no cast.
/// Read by `direct_helper_table_for` and the OSR door.
pub fn varhandle_read_slot_bindable(slot: usize) -> bool {
    cratonvm_jit::varhandle_read_slot_kind(slot) != cratonvm_jit::VARHANDLE_READ_KIND_REF_STRICT
        || !cratonvm_native_builtins::lang_invoke::vh_reference_return_checkcast_enabled()
}





/// Would a spliced body's `ByteBuffer.get(int)` / `put(int, byte)` call
/// (`write`) have been bound to the thin `DirectByteBuffer` element helper by
/// the callee's own compile -- the reason the VM resolver refuses to splice a
/// body holding one (`thin-bound-native-keeps-its-own-fast-path`)?
///
/// The binds' own two questions, asked without a call-site profile (the call
/// is inside the callee): the registry admits the implementing triple
/// (`--real-jdk`), and the helper has served a `DirectByteBuffer` in that
/// direction (`elem_fastpath::class_is_served`, the table the bind gate
/// `nio_byte_element_bind_refused` and the helper's own prologue read). A
/// helper that has never served the class binds nowhere, so the splice loses
/// nothing. Compile time only.
pub fn nio_byte_splice_keeps_thin_bind(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    write: bool,
) -> bool {
    if !cratonvm_jit::nio_byte_direct_helpers_enabled() {
        return false;
    }
    let (name, desc) = if write {
        ("put", "(IB)Ljava/nio/ByteBuffer;")
    } else {
        ("get", "(I)B")
    };
    // `--compatible` asks too once the bind does (SYNC22-3, JIT r23 w1 L6,
    // `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY`): a bind the door refuses keeps
    // nothing for the splice to lose.
    if (crate::vm::dispatch_policy(shared).is_jdk_only()
        || compat_bind_asks_registry_enabled(shared))
        && !direct_native_bind_admitted(shared, "java/nio/DirectByteBuffer", name, desc)
    {
        return false;
    }
    ["java/nio/DirectByteBuffer", "java/nio/DirectByteBufferR"]
        .iter()
        .filter_map(|class| cm.get_loaded_class_id(class))
        .any(|id| {
            cratonvm_native_io::direct_buffer::elem_fastpath::class_is_served(id.as_u32(), write)
        })
}

/// [`nio_byte_splice_keeps_thin_bind`] for `MessageDigest.update(byte)`: the
/// switch and the `--real-jdk` admission the bind asks
/// (`md_update_direct_helper_enabled`, `direct_native_helper`). Compile time
/// only.
pub fn md_update_splice_keeps_thin_bind(shared: &SharedVm) -> bool {
    // `--compatible` asks the door's question too once the bind does
    // (SYNC22-3, JIT r23 w1 L6, `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY`).
    let asks = crate::vm::dispatch_policy(shared).is_jdk_only()
        || compat_bind_asks_registry_enabled(shared);
    cratonvm_jit::md_update_direct_helper_enabled()
        && (!asks
            || direct_native_bind_admitted(shared, "java/security/MessageDigest", "update", "(B)V"))
}

pub fn varhandle_splice_keeps_thin_bind(
    shared: &SharedVm,
    name: &str,
    desc: &str,
    ir_mode: bool,
) -> bool {
    if !cratonvm_native_builtins::lang_invoke::vh_splice_refusal_asks_bind_enabled() {
        return cratonvm_jit::varhandle_read_helper_slot(name, desc).is_some()
            || cratonvm_jit::varhandle_byte_array_view_read_slot(name, desc).is_some()
            || cratonvm_jit::varhandle_write_helper_slot(name, desc).is_some();
    }
    // The optimizing graph binds primitive reads in relocated bodies too,
    // with an artifact-owned judged descriptor. Preserve C1 protection for
    // every family that still lacks that IR binding.
    if ir_mode
        && cratonvm_jit::varhandle_ir_read_shape(name, desc)
        && varhandle_method_entry_binds_admitted(shared)
        && direct_native_bind_admitted(
            shared,
            "java/lang/invoke/VarHandle",
            name,
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )
    {
        return false;
    }
    // JIT round 20 wave 11 (lane pIndy, W14L4-2 = W12L2-3;
    // `CRATONVM_JIT_SPLICE_VH_IR_BOUND`, default ON, `=0` restores the
    // refusal): an optimizing-tier splice keeps the bind too when the IR
    // tier's post-build bind loop binds the spliced call to the same thin
    // helper (`cratonvm_jit::varhandle_ir_bound_family`: primitive reads,
    // writes, `compareAndSet`), the helper table is wired on this VM (the
    // exact-handle gate) and the registry admits the bind on the erased
    // descriptor. A constant handle's access is then served inline by the
    // optimizing tier's prefix, in the caller: `CompletableFuture.tryPushStack`
    // / `completeValue`, `ConcurrentLinkedQueue`'s `NEXT` / `ITEM` CASes.
    if ir_mode {
        if let Some(erased) = cratonvm_jit::varhandle_ir_bound_family(name, desc) {
            if varhandle_method_entry_binds_admitted(shared)
                && direct_native_bind_admitted(shared, "java/lang/invoke/VarHandle", name, erased)
            {
                return false;
            }
        }
    }
    // Round 15 wave 6 (lane vh8): the ladder's own gate, judged binds included.
    if !cratonvm_jit::direct_jit_callee_calls_enabled()
        || !varhandle_method_entry_binds_admitted(shared)
    {
        return false;
    }
    let erased = if cratonvm_jit::varhandle_read_direct_helpers_enabled()
        && (cratonvm_jit::varhandle_read_helper_slot(name, desc)
            .is_some_and(varhandle_read_slot_bindable)
            || cratonvm_jit::varhandle_byte_array_view_read_slot(name, desc).is_some())
    {
        "([Ljava/lang/Object;)Ljava/lang/Object;"
    } else if cratonvm_jit::varhandle_write_direct_helpers_enabled()
        && cratonvm_jit::varhandle_write_helper_slot(name, desc).is_some()
    {
        "([Ljava/lang/Object;)V"
    } else if cratonvm_jit::varhandle_cas_direct_helpers_enabled()
        && cratonvm_jit::varhandle_cas_helper_slot(name, desc).is_some()
    {
        "([Ljava/lang/Object;)Z"    } else {
        return false;
    };
    direct_native_bind_admitted(shared, "java/lang/invoke/VarHandle", name, erased)}


/// The exact-`VarHandle` judgement of `jit_invoke_dispatch`'s generic tail
/// (interpreter round i1 wave 38, lane L4): the refusal raised as the site's
/// pending exception when the receiver (`args_slice[0]`) has its `exact`
/// field (at `exact_slot`) set and `info`, the site's real descriptor, is not
/// its access mode type; `None` for every other receiver and site. The twin
/// of the check in `try_jit_site_cached_native_dispatch`, for a site that
/// cache declined. `CRATONVM_DBG_MH_STACK=1` prints its
/// `[MH_STACK] exact VarHandle` line.
///
/// # SAFETY
///
/// `args_slice[0]` is a raw reference from compiled code; it is validated as
/// a heap object before it is read.
#[cold]
#[inline(never)]
pub unsafe fn jit_var_handle_exact_refusal(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    exact_slot: usize,
) -> Option<i64> {
    let vh = jit_exact_var_handle_receiver(vm, args_slice, exact_slot)?;
    let (thread, _guard) = jit_thread_mut()?;
    let error = jit_var_handle_exact_judgement(vm, thread, info, vh)?;
    Some(handle_jit_dispatch_error(vm, thread, error, info))
}

/// The receiver `args_slice[0]` of a compiled `VarHandle` access when it is a
/// heap object whose `exact` field (at `exact_slot`) is set, else `None`.
///
/// # SAFETY
///
/// `args_slice[0]` is a raw reference from compiled code; it is validated as
/// a heap object before it is read.
pub unsafe fn jit_exact_var_handle_receiver(
    vm: &SharedVm,
    args_slice: &[i64],
    exact_slot: usize,
) -> Option<ObjectRef> {
    let raw = *args_slice.first()? as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let vh = vm.mem.heap.is_object_address(raw as usize)?;
    crate::runtime::interpreter::var_handle_receiver_is_exact(vm, vh, exact_slot).then_some(vh)
}

/// `CRATONVM_DBG_MH_STACK=1` (round 15 wave 4, lane vh6;
/// `r15w4-vh5-via-caller-keeps-thin-bind-after-mint-FIXED-20260930.md`): one
/// `[MH_STACK] exact VarHandle ... UNJUDGED` line per call of a thin
/// `VarHandle` helper (either arm) whose handle is invoke-exact once this VM
/// has minted one. No door judges such an access (the helper has no site
/// descriptor, and its cold arm's stand-in is judged only when its call site
/// is recovered: [`jit_var_handle_exact_judgement`], interpreter round i1
/// wave 49, lane L4, so a cold-arm line may also be judged), and no compile made
/// after the first mint binds a helper, so every line is a body bound BEFORE
/// the mint still being run: the midLoop residual, or whatever the first-mint
/// sweep missed. Counted against a probe's wrong calls, the lines split the
/// `viaCaller` candidates (thin helper vs. an unjudged door), beside the judged
/// doors' own `[MH_STACK] exact VarHandle <name><desc>: refused|admitted` lines.
///
/// # SAFETY
///
/// `vh` is a raw reference from compiled code; it is validated as a heap
/// object before it is read.
#[cold]
#[inline(never)]
pub unsafe fn note_thin_varhandle_helper_unjudged(vm: &SharedVm, vh: i64, family: &str, slot: usize) {
    let Some(exact_slot) = crate::runtime::interpreter::var_handle_exact_gate(vm) else {
        return;
    };
    if jit_exact_var_handle_receiver(vm, &[vh], exact_slot).is_none() {
        return;
    }
    eprintln!(
        "[MH_STACK] exact VarHandle UNJUDGED: thin {family} helper slot {slot}, bound before \
         the first exact mint"
    );
}

/// The exact-`VarHandle` judgement of `jit_invoke_virtual_mic`, the door
/// compiled `invokevirtual` sites reach (round 15 wave 3, lane vh3;
/// `r15w2-vh2-thin-helpers-bound-before-first-exact-mint-FIXED-20260930.md`).
///
/// The twin of [`jit_var_handle_exact_refusal`] for that door, which had
/// none: once the native site cache declined a `VarHandle` site (its receiver
/// guard names another handle class, a cached refusal, the cache switched
/// off), every arm after it ends in `invoke_or_native`, whose
/// signature-polymorphic block serves the access unjudged. Under `--real-jdk`
/// (no thin binds) that was how a method compiled before the first exact mint
/// accepted every mistyped access to an exact handle after its first
/// (`R15Vh2ExactThinBind` road `late`: `wmte=1 accepted=2499`, HotSpot
/// `wmte=2500 accepted=0`).
///
/// Takes the caller's thread borrow (`jit_invoke_virtual_mic_body` holds its
/// guard for the whole call; a second `jit_thread_mut` here would be a
/// sibling borrow). The stand-in sites of the thin helpers never reach this
/// door. `CRATONVM_JIT_VH_MIC_EXACT_JUDGE=0` stands it down.
///
/// # SAFETY
///
/// `args_slice[0]` is a raw reference from compiled code; it is validated as
/// a heap object before it is read.
#[cold]
#[inline(never)]
pub unsafe fn jit_mic_var_handle_exact_refusal(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args_slice: &[i64],
) -> Option<i64> {
    if is_varhandle_stand_in_info(info) {
        return None;
    }
    let exact_slot = crate::runtime::interpreter::var_handle_exact_gate(vm)?;
    let vh = jit_exact_var_handle_receiver(vm, args_slice, exact_slot)?;
    if !cratonvm_native_builtins::lang_invoke::vh_exact_mic_judge_enabled() {
        return None;
    }
    let error = crate::runtime::interpreter::var_handle_exact_refusal(
        vm,
        &mut *thread,
        vh,
        info.method_name,
        info.descriptor,
    )?;
    Some(handle_jit_dispatch_error(vm, thread, error, info))
}

/// Withdraw every compiled body of `shared` that bound a thin `VarHandle`
/// helper (round 15 wave 3, lane vh3; VH2-2 of `jit-r15w2-vh2-proposals-RETIRED-20261003.md`,
/// `r15w2-vh2-thin-helpers-bound-before-first-exact-mint-FIXED-20260930.md`).
///
/// Called once per VM, at its FIRST exact `VarHandle` mint (the
/// `u32::MAX -> slot` transition of `NativeRealm::var_handle_exact_slot`, in
/// `vm_exec`'s `note_exact_var_handle_minted`), AFTER the gate is stored: a
/// compile that reads the helper table from then on binds nothing
/// ([`varhandle_thin_binds_admitted`]), and every body that bound before it
/// named `cratonvm_jit::VARHANDLE_THIN_BIND_DEPENDENCY` in its
/// `inlined_methods`. `JitCache::invalidate_for_class` then withdraws those
/// bodies and their raw direct callers, retargets the inline caches that held
/// them, and logs the name, so a thin-bound compile still in flight is refused
/// at publication. A caller's next call re-dispatches, and the method
/// recompiles without the binds. An activation already running such a body
/// (round 15 wave 5, lane vh7) leaves for the interpreter at its next back
/// edge, or when the call it is inside returns: the sweep marks the bodies it
/// withdrew and forces their exits (`JitCache::invalidate_varhandle_thin_binds`,
/// `CRATONVM_JIT_VH_MINT_FORCES_EXITS`); a body with no forceable exit still
/// finishes on its binds.
///
/// Returns the count withdrawn; `0` under
/// `CRATONVM_JIT_VH_THIN_BIND_RETIRE=0`.
pub fn retire_varhandle_thin_bound_bodies(shared: &SharedVm) -> usize {
    if !cratonvm_jit::varhandle_thin_bind_retire_enabled() {
        return 0;
    }
    // Round 15 wave 5 (lane vh7): the sweep makes the thin-bound method-entry
    // bodies not entrant (`CRATONVM_JIT_VH_MINT_NOT_ENTRANT`), which patches
    // nothing until the cache holds this VM's hook. Armed exactly as the
    // redefinition path arms it (`redefine_class_with`); idempotent.
    if cratonvm_jit::varhandle_mint_not_entrant_enabled() {
        shared
            .jit
            .jit_cache
            .arm_not_entrant(cratonvm_jit::NotEntrantHook {
                // Cast: a helper's address, baked into generated code.
                helper: jit_not_entrant_entry as *const () as usize,
                // Cast: this VM's address, the helpers' `vm_ptr` word.
                vm_ptr: shared as *const SharedVm as usize,
            });
    }
    shared.jit.jit_cache.invalidate_varhandle_thin_binds().0
}

/// Must `artifact` be DROPPED instead of published (round 15 wave 3, lane
/// vh4; VH3-1 of `jit-r15w3-vh3-proposals-RETIRED-20261003.md`)? `true` when it bound a thin
/// `VarHandle` helper (it names `cratonvm_jit::VARHANDLE_THIN_BIND_DEPENDENCY`)
/// and this VM no longer admits those binds ([`varhandle_thin_binds_admitted`]:
/// an exact handle has been minted and the gate is on).
///
/// The window it closes (`r15w3-vh3-running-thin-bound-activation-keeps-binds-FIXED-20260930.md`,
/// "Second, narrower window"): a compile reads the helper table
/// (`direct_helper_table_for`) while the gate is closed; another thread mints,
/// stores the gate and runs the sweep, which logs its invalidation record;
/// only then does the compile open its scope. Its body binds, is stamped
/// LATER than the record, and `JitCache::put` accepts it: a thin-bound body
/// published after the sweep, which nothing withdraws. Any thin-bound artifact
/// that reaches a publishing door once binds are no longer admitted was built
/// from a table read before the gate opened, so dropping it is exact (never a
/// body a current compile would make) and always sound (the next trigger
/// recompiles without binds). A mint AFTER this check is the sweep's: its
/// record is then newer than the compile's scope, and `put` refuses the body.
///
/// Cost at a publishing door: one relaxed load for a VM that never minted an
/// exact handle; otherwise a scan of `inlined_methods` (empty for almost every
/// body). `CRATONVM_JIT_VH_THIN_BIND_PUBLISH_REFUSAL=0` publishes it (the
/// round-15-wave-3 behaviour); read only when the answer would be `true`, so
/// no cached flag. Called by each publishing door just before its `put` /
/// `put_osr` (`r15w3-vh4-thin-bound-publication-doors-patch-FIXED-20260930.md`).
pub fn varhandle_thin_bound_artifact_is_stale(
    shared: &SharedVm,
    artifact: &cratonvm_jit::CompiledMethod,
) -> bool {
    if varhandle_thin_binds_admitted(shared) {
        return false;
    }
    artifact
        .inlined_methods
        .iter()
        .any(|(class_name, _, _)| class_name == cratonvm_jit::VARHANDLE_THIN_BIND_DEPENDENCY)
}


/// Judge the exact `VarHandle` `vh` at the compiled site `info`: with
/// `info`'s own descriptor, or, for a thin helper's stand-in site, with the
/// descriptor its compiled caller's invoke names
/// ([`varhandle_stand_in_site_descriptor`]). A stand-in whose real site
/// cannot be recovered is not judged (the pre-wave-49 answer), which
/// `CRATONVM_DBG_MH_STACK=1` reports.
#[cold]
#[inline(never)]
pub fn jit_var_handle_exact_judgement(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    vh: ObjectRef,
) -> Option<crate::error::MethodCallFailed> {
    let recovered;
    let site: &str = if is_varhandle_stand_in_info(info) {
        recovered = if vm.classes.vh_exact_site_memo {
            varhandle_stand_in_site_descriptor_memoized(vm, thread, info)
        } else {
            varhandle_stand_in_site_descriptor(vm, info)
        };
        match recovered.as_deref() {
            Some(desc) => desc,
            None => {
                if crate::runtime::env_cache::dbg_mh_stack() {
                    eprintln!(
                        "[MH_STACK] exact VarHandle {}: stand-in site not recovered, not judged",
                        info.method_name
                    );
                }
                return None;
            }
        }
    } else {
        info.descriptor
    };
    crate::runtime::interpreter::var_handle_exact_refusal(vm, thread, vh, info.method_name, site)
}

/// How many sites [`varhandle_stand_in_site_descriptor_memoized`] remembers
/// per thread.
pub const VH_EXACT_SITE_MEMO_CAP: usize = 8;

/// [`varhandle_stand_in_site_descriptor`] through the thread's memo
/// (`CRATONVM_VH_EXACT_SITE_MEMO`; interpreter round i1 wave 50, lane L4,
/// `i49-L4-proposal-memoize-the-exact-varhandle-site-recovery`). The key is
/// the innermost compiled activation as the inline frame record names it
/// (`conservative_roots::innermost_compiled_site_key`: compile id, safepoint
/// id, method) and the stand-in site; a hit also compares the method's label,
/// so a recycled compile id and address naming another method misses. The
/// safepoint id fixes the invoke (and its inlined level) within the method,
/// so a key names one call site and one descriptor. Without an active frame
/// record the walk runs, as before. `CRATONVM_DBG_MH_STACK=1` prints the
/// walk's `stand-in site` line once per site (the positive control), not
/// once per access.
#[cold]
#[inline(never)]
pub fn varhandle_stand_in_site_descriptor_memoized(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
) -> Option<String> {
    let Some((id, sp_id, cm_ptr)) = crate::jit::conservative_roots::innermost_compiled_site_key()
    else {
        return varhandle_stand_in_site_descriptor(vm, info);
    };
    // SAFETY: see `innermost_compiled_site_key`: the activation is live below
    // this helper on this thread.
    let label: &str = unsafe { &(*cm_ptr).method_label };
    let key = (
        id,
        sp_id,
        cm_ptr as usize,
        info as *const JitInvokeInfo as usize,
    );
    let memo = &mut thread.vh_exact_site_memo;
    if let Some(pos) = memo.iter().position(|e| e.key == key && &*e.label == label) {
        if pos != 0 {
            let hit = memo.remove(pos);
            memo.insert(0, hit);
        }
        return Some(memo[0].descriptor.to_string());
    }
    let descriptor = varhandle_stand_in_site_descriptor(vm, info)?;
    let memo = &mut thread.vh_exact_site_memo;
    memo.truncate(VH_EXACT_SITE_MEMO_CAP - 1);
    memo.insert(
        0,
        crate::threading::jvm_thread::VhExactSiteMemo {
            key,
            label: label.into(),
            descriptor: descriptor.as_str().into(),
        },
    );
    Some(descriptor)
}

/// The site-cached dispatch's error path for a `VarHandle` access whose native
/// refused a parameter at `asType` (interpreter round i2 wave 53, lane L4;
/// `vm_exec::vh_astype_refusal_site_text`): the message names the call site's
/// descriptor -- for a thin helper's stand-in site the one recovered from the
/// compiled invoke, as the exact-type judgement recovers it, and no rewrite
/// when it cannot be recovered (a stand-in's erased coordinate is not the
/// site's type).
#[cold]
#[inline(never)]
pub fn jit_vh_astype_refusal_site_text(
    vm: &SharedVm,
    thread: &mut JvmThread,
    failed: crate::error::MethodCallFailed,
    info: &JitInvokeInfo,
) -> crate::error::MethodCallFailed {
    let recovered;
    let site: &str = if is_varhandle_stand_in_info(info) {
        recovered = if vm.classes.vh_exact_site_memo {
            varhandle_stand_in_site_descriptor_memoized(vm, thread, info)
        } else {
            varhandle_stand_in_site_descriptor(vm, info)
        };
        match recovered.as_deref() {
            Some(d) => d,
            None => return failed,
        }
    } else {
        info.descriptor
    };
    crate::vm::vm_exec::vh_astype_refusal_site_text_cold(vm, thread, failed, info.method_name, site)
}

/// The descriptor of the `invokevirtual VarHandle.<mode>` a thin `VarHandle`
/// helper's stand-in site `info` stands for (interpreter round i1 wave 49,
/// lane L4; `interpreter-L4-a-compiled-varhandle-site-skips-the-invoke-exact-check-FIXED-20261013`).
///
/// A baked thin call has no `JitInvokeInfo` of its own, so its cold arm
/// dispatches with a per-slot stand-in whose coordinate is erased; an
/// invoke-exact handle must be judged against the call site's exact type.
/// The helper is called straight from compiled code, which publishes the
/// invoke's safepoint id first, so the current thread's innermost compiled
/// activation (its innermost inlined level when the site was inlined) names
/// the method and bci of that invoke; the descriptor is read from its
/// constant pool. `None`, never a guess, when the bci is unknown, the
/// instruction there is not an `invokevirtual` of a `VarHandle` access mode
/// of `info`'s name, or the method is gone. Cold: only an exact receiver at a
/// stand-in site asks.
#[cold]
#[inline(never)]
pub fn varhandle_stand_in_site_descriptor(vm: &SharedVm, info: &JitInvokeInfo) -> Option<String> {
    let frames = crate::jit::conservative_roots::active_compiled_frames();
    let top = frames.last()?;
    let (label, bci, class_id) = match top.inline_chain.first() {
        Some(level) => (&*level.label, i64::from(level.bci), level.class_id),
        None => (&*top.label, i64::from(top.bci), top.owner_class_id),
    };
    if bci < 0 || class_id == 0 {
        return None;
    }
    let bci = bci as usize;
    let (_, method_name, method_desc) = split_method_label(label)?;
    let cm = vm.classes.class_manager.read_recursive();
    let class = cm.get_class(ClassId::new(class_id))?;
    let code = class.find_method(&method_name, &method_desc)?.code()?;
    let bytes = code.code.as_bytes();
    // `invokevirtual indexbyte1 indexbyte2`.
    if bytes.get(bci).copied()? != 0xb6 {
        return None;
    }
    let cp_index = u16::from_be_bytes([*bytes.get(bci + 1)?, *bytes.get(bci + 2)?]);
    let cratonvm_reader::constant_pool::ConstantPoolEntry::MethodReference {
        class_index,
        name_and_type_index,
    } = class.constant_pool.get(cp_index)?
    else {
        return None;
    };
    let owner = class.constant_pool.get_class_name(*class_index)?;
    let (name, desc) = class
        .constant_pool
        .get_name_and_type(*name_and_type_index)?;
    if name != info.method_name
        || !crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(owner)
    {
        return None;
    }
    if crate::runtime::env_cache::dbg_mh_stack() {
        eprintln!("[MH_STACK] exact VarHandle stand-in site -> {label} bci {bci}: {name}{desc}");
    }
    Some(desc.to_string())
}

/// Synthetic call sites for the cold arm of the `VarHandle` CAS helpers, one
/// per slot. `static` for the same reason [`VARHANDLE_WRITE_INFOS`] is:
/// `jit_invoke_dispatch` keys its per-site memos on `(vm_identity, info
/// address)`, so the address has to be process-stable.
pub static VARHANDLE_CAS_INFOS: [JitInvokeInfo; cratonvm_jit::VARHANDLE_CAS_SLOTS] = [
    vh_cas_info(0),
    vh_cas_info(1),
    vh_cas_info(2),
    vh_cas_info(3),
    vh_cas_info(4),
    vh_cas_info(5),
    vh_cas_info(6),
    vh_cas_info(7),
    vh_cas_info(8),
    vh_cas_info(9),
];

/// One entry of [`VARHANDLE_CAS_INFOS`]. `num_jit_args: 4` counts the receiver
/// — the `VarHandle` itself — plus the coordinate, `expected` and `new`.
pub const fn vh_cas_info(kind: usize) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/VarHandle",
        method_name: "compareAndSet",
        descriptor: VARHANDLE_CAS_DESCRIPTORS[kind],
        num_jit_args: 4,
        return_type: b'Z',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}


/// Try to compile a callee method from a JitInvokeInfo.
/// Returns (entry_ptr, needs_context) if compilation succeeds.
// SAFETY: Caller must ensure vm is a valid SharedVm reference and info points to a live
// JitInvokeInfo. Delegates to try_jit_compile_callee which accesses the class manager
// and JIT compiler; no raw pointer dereferences occur within this function itself.
#[allow(clippy::type_complexity)]
pub unsafe fn try_compile_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    // JIT-dispatch callee compile — optimized (C2-equivalent) tier. The
    // artifact comes back with the entry so the caller can keep it mapped for
    // as long as it calls or caches that address. The call site's own class
    // (`declaring_class_id`) names which loader's `info.class_name` it means;
    // a substituted owner is compiled for its recorded id.
    if let Some(owner) = jit_substituted_owner(vm, info) {
        return crate::runtime::interpreter::try_jit_compile_callee_for_class(
            vm,
            owner,
            info.class_name,
            info.method_name,
            info.descriptor,
            true,
        );
    }
    crate::runtime::interpreter::try_jit_compile_callee_from_site(
        vm,
        info.declaring_class_id,
        info.class_name,
        info.method_name,
        info.descriptor,
        true,
    )
}

/// Synthetic call-site info for [`jit_integer_value_of_direct`]'s error
/// path: `handle_jit_dispatch_error` only reads the name triple for
/// diagnostics/exception context, and this helper serves exactly one callee.
pub static INTEGER_VALUE_OF_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Integer",
    method_name: "valueOf",
    descriptor: "(I)Ljava/lang/Integer;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// ---------------------------------------------------------------------------
// Thin direct-call native helpers (`jit_integer_value_of_direct`,
// `jit_integer_int_value_direct`, `jit_hashmap_put_direct`,
// `jit_hashmap_get_direct`, `jit_string_latin1_to_lower_direct`,
// `jit_concurrent_hashmap_get_direct`).
//
// Each is a VM-side reimplementation of a registered native, baked straight
// into the emitted `CALL` — no dispatch helper, and so no policy check, on the
// path.
//
// # H12-1: the two gates this comment used to claim were BOTH false
//
// What stood here until 2026-08-20 said these bodies need no internal Real-JDK
// check because "under `JdkOnly` they are unreachable, gated twice and both
// gates upstream of any code that could execute", naming:
//
//   1. `build_helpers` does not register their addresses under `JdkOnly`, so
//      the `*_DIRECT_FN` cells stay `0`; and
//   2. `jit::direct_native_helper` refuses to bind a non-zero address.
//
// **Gate 1 has not existed since 2026-08-06.** `jit/src/lib.rs`'s own module
// comment states the opposite in as many words: *"the `*_DIRECT_FN` helper
// addresses are registered **unconditionally** (2026-08-06) … and the bind
// decision is threaded per compilation as an argument instead"*. The paragraph
// above was not updated when its own premise was deleted. `jit/src/lib.rs`'s
// JDK-ONLY-NOTE item 3 still lists withholding them as an OPEN ask, which is
// the same fact from the other side.
//
// **Gate 2 exists and works — at ONE of the three compile doors.**
// `direct_native_helper` is called only from `try_compile_inner`, i.e.
// `CompileDoor::MethodEntry` (both its single-pass and IR ladders). The other
// two doors in `jit::compile_gate`'s three-door table reach
// `x64::compile_with_param_slots` directly and carry their own hand-copied
// direct-call ladders, neither of which asks any policy question:
//
//   * `CompileDoor::Osr` — `vm/src/runtime/interpreter/jit_bridge.rs`, which
//     binds `jit_hashmap_get_direct`, `jit_hashmap_put_direct`,
//     `jit_thread_current_thread_direct`, `jit_preconditions_check_index_direct`
//     and `jit_reachability_fence_direct`;
//   * `CompileDoor::EagerFirstCall` — `vm/src/runtime/interpreter.rs`, which
//     binds `jit_integer_value_of_direct` and `jit_integer_int_value_direct`.
//
// Both take the helper's address as `crate::jit::helpers::NAME as *const () as
// usize` rather than reading the `*_DIRECT_FN` cell, and both say in a comment
// that they do so *because* `build_helpers` publishes those cells too late for
// the first compile. So gate 1 would not have covered them even when it
// existed, and gate 2 never saw them.
//
// MEASURED, `--real-jdk`, binary at fe59bf9d9, `CRATONVM_INTRINSIC_STATS=1`, a
// loop inside a once-invoked method:
//
//     compiled Thread.currentThread direct calls: 298000
//       (sites bound per compile door: single-pass 0/7, IR 0/0, OSR 1)
//
// The MethodEntry door examined seven `invokestatic` sites and refused all
// seven; the OSR door bound one and compiled code then called it 298 000
// times. `java/lang/Thread.currentThread()Ljava/lang/Thread;` is `bridge` in
// `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, and so are `HashMap.get`,
// `HashMap.put`, `Preconditions.checkIndex` and `Reference.reachabilityFence`.
// A `--fast-boot` run of the same probe reports the identical `OSR 1`: the OSR
// door does not know which mode it is in.
//
// # Why the check is per-invocation and here, rather than at the binds
//
// A bind-time check has to be written once per door, and the record of this
// file is that a fourth door — or a fifth ladder in an existing one — arrives
// before anyone revisits the gate. `compile_gate`'s own module doc calls that
// out: three doors, and each grew its hand-copied subset of the admission
// checks "reactively after its own bug". A check in the callee is the one
// shape that cannot be forgotten by a door that has not been written yet.
//
// It costs one relaxed policy read per call, on the compatible arm, ahead of
// work that already includes an object-address probe and a class comparison.
// That cost is NOT measured — see the H12-1 record's verification plan, which
// names the A/B that must run before this is called free.
//
// This does not replace gate 2. `direct_native_helper` is still the right
// place to refuse, because a refusal there costs nothing at run time; this is
// the backstop for the doors that never ask it.
//
// # 2026-09-22 — what "refused" was worth, and what both gates now ask
//
// Everything above is a correct account of WHERE the question was asked and
// WHO forgot to ask it. What neither gate had established is that the answer
// was worth anything, and it was not.
//
// A refusal at either gate routes the call to `jit_invoke_dispatch`, whose
// tail is `vm_exec::invoke_or_native`. That function computes §7 step 3's
// input as `has_real || dial` and evaluates `has_real` **only** for a
// `SyntheticStub` on `real_protected_stub_class` — so for a `Bridge` it is
// structurally `false`, and the tail ran the very native the gate had just
// refused to let a thin helper re-implement. `H17-3` §6 already said so in
// that function's own words. Every `bridge` row the paragraphs above name —
// `Thread.currentThread`, `HashMap.get`, `HashMap.put`, `ConcurrentMap.get`,
// `Preconditions.checkIndex`, `Reference.reachabilityFence` — was in that
// case. The refusal bought no policy; it swapped an inlined VM-side body for
// the same VM-side body reached through a funnel, and charged the funnel.
//
// MEASURED on the per-byte `MessageDigest.update(byte)` loop of
// `apps/probes/MdUpdateByteLoopProbe.java`: `--compatible` with the helper
// switched off measured what `--real-jdk` measured with it on, i.e. the whole
// remaining gap between the modes on that loop was this refusal. See
// `docs/feature-designs/jit-ir-optimizations.md`.
//
// So both gates now ask the tail's question, through ONE predicate
// (`direct_helper_refusal`): bind time via `direct_native_bind_admitted`,
// call time via `jit_direct_helper_refused` and its per-helper
// `DirectHelperGate`. A refusal at either now means the tail refuses too —
// an unregistered triple, a `SyntheticStub`, a capability-classified row, or
// an armed `CRATONVM_ENFORCE_NATIVE_SHADOW`. The callee-side backstop stays,
// for the reason this block gives two paragraphs up; what changed is that its
// answer is no longer a constant.
//
// JDK-ONLY-WAVE2 §4, layering half CLOSED 2026-08-06/08-10. The ask was that
// the compile-time recognition in `jit::try_compile` consult the shared
// resolver about the triple's registered native, instead of the JIT crate
// carrying a latched process-global mirror of a policy it could not evaluate
// because it cannot see `NativeKind`. Both halves landed:
// `direct_native_helper` takes an `direct_bind_admission` the VM answers out of
// the registry's kind, and the policy is threaded per compilation rather than
// read from the latch. Gate 2 is that; gate 1 is gone.
// ---------------------------------------------------------------------------

/// Times a thin direct-call helper declined to serve its own fast path because
/// the calling VM is in `JdkOnly` **and the generic tail that decline routes
/// to would refuse as well**.
///
/// # What changed 2026-09-22, and why every earlier reading of this is wrong
///
/// Until 2026-09-22 this counted *every* call into a guarded helper under
/// `JdkOnly`, because [`jit_direct_helper_refused`] refused unconditionally in
/// strict mode. That refusal prevented nothing. The decline routes the call to
/// `jit_invoke_dispatch`, whose tail is `vm_exec::invoke_or_native`, and
/// `invoke_or_native` computes §7 step 3's input as `has_real || dial` with
/// `has_real` evaluated **only** for a `SyntheticStub` on
/// `real_protected_stub_class` — so for a `Bridge` it is structurally `false`
/// and the tail **ran the very native the helper had just refused to
/// re-implement**. `H17-3` §6 says so in that function's own words.
///
/// A pre-2026-09-22 non-zero reading here is therefore a count of *detours*,
/// not of blocks: the same code ran, through the slowest route in the VM, at
/// ~3x on the per-byte `MessageDigest.update(byte)` loop
/// `docs/feature-designs/jit-ir-optimizations.md`
/// measures. It is the same defect the sibling page fixed at
/// [`jdk_only_admit_jit_fast_native`], at a second door.
///
/// # Reading a zero, now
///
/// A refusal counted here now means the tail refuses too, so the pairing with
/// a second counter still matters and the readings have changed:
///
/// | this | [`jit_direct_helper_jdk_only_admissions`] | meaning |
/// |---|---|---|
/// | 0 | 0 | no compiled site executed a guarded helper — the run says nothing |
/// | 0 | >0 | strict mode ran the helpers, and the tail would have run the same natives |
/// | >0 | any | a guarded triple is unregistered, a `SyntheticStub`, capability-classified, or `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed over it |
///
/// The third row is the only one that is a block. It is rare by construction
/// on a stock `--real-jdk` run, which is why the admissions counter beside it
/// exists: without it the healthy state and "no compiled site ever got here"
/// are the same zero, which is the reading mistake [`jit_funnel_bypass_count`]'
/// doc describes for inert fast paths.
///
/// `vm`'s own count, summed over the per-thread tallies registered with its
/// `JitCache::jdk_only_sinks` (round 11: this was a process-global
/// `JIT_DIRECT_HELPER_JDK_ONLY_REFUSALS` static bumped per call; until t1 wave
/// 6, lane Y3, a sum over every thread's [`JitCounterBlock`] in the process,
/// so a VM's `--real-jdk-report` counted every other VM's calls too).
pub fn jit_direct_helper_jdk_only_refusals(vm: &SharedVm) -> u64 {
    vm.jit.jit_cache.jdk_only_sinks().direct_helper_refusals()
}

/// Times a guarded thin direct-call helper was **admitted** under `JdkOnly`
/// because the generic tail would have run the same registered native.
///
/// The denominator [`jit_direct_helper_jdk_only_refusals`] needs; see that
/// counter's table. A zero there is ambiguous between "the policy question
/// never came up" and "it came up and the answer was yes", and only this
/// number separates them. `vm`'s own count, like the refusals.
pub fn jit_direct_helper_jdk_only_admissions(vm: &SharedVm) -> u64 {
    vm.jit.jit_cache.jdk_only_sinks().direct_helper_admissions()
}

/// [`jit_direct_helper_jdk_only_refusals`] over every VM the process ran, for
/// the process-wide instrument census (`CRATONVM_INSTRUMENT_CENSUS`) only;
/// no VM's report reads it. Sums every tally every thread registered.
pub fn jit_direct_helper_jdk_only_refusals_in_process() -> u64 {
    jit_counter_total(|b| {
        b.direct_helper_tallies
            .lock()
            .iter()
            .map(|(_, t)| t.refusals())
            .sum()
    })
}

// ===========================================================================
// The callee-side `--real-jdk` gate for the thin direct-call helpers.
//
// `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`,
// §"What would settle it" item 1: *thread each helper's triple into
// `jit_direct_helper_refused` and reuse `jit_fast_native_bytecode_terms` — the
// predicate the sibling fix added, which already mirrors `invoke_or_native`'s
// two terms. A refusal then means the tail refuses too.*
//
// The triple is precisely what the old predicate lacked. It took only
// `&SharedVm`, so it could not ask the registry anything and had exactly one
// answer per mode. [`DirectHelperGate`] is that triple, carried as a `static`
// beside the helper that owns it, with the per-triple memo that keeps asking
// cheap enough to do per call (since nio21 wave 8 L2 the memo is per VM, in
// `JitRealm::direct_helper_gates`, at the gate's ordinal).
// ===========================================================================

/// One guarded thin direct-call helper's `--real-jdk` policy gate: the triple
/// whose registered native its fast path re-implements, plus the ordinal of
/// the per-VM memo that makes asking about it a relaxed load.
///
/// # Why a `static` per helper rather than an enum with a table
///
/// [`DirectNativeShadow`] is the bind-time enumeration and that is the right
/// shape there: a compile door holds no site state, so the variant IS the
/// identity. A callee-side gate holds state — the memoised `NativeMethodId` —
/// and the natural home for per-site state is the site. Declaring the gate
/// next to the helper's own `JitInvokeInfo` also puts the one thing that can
/// silently go wrong in one glance: the gate and the decline edge must name
/// the **same** dispatch, or the helper is being policed about a triple it
/// does not stand in front of.
///
/// nio21 wave 8 L2: the STATE moved out. A memo of one registry's resolution
/// is per-VM state, which AGENTS.md keeps out of process statics, so the
/// memo now lives at `vm.jit.direct_helper_gates.site(ordinal)` and the
/// `static` is identity plus `const` metadata. The pairing argument above is
/// unchanged: the gate is still declared beside its helper.
///
/// # The triple is the one whose native RUNS, not the one the site names
///
/// H7-1's rule, which [`DirectNativeShadow::triple`] states for the bind-time
/// half. `jit_concurrent_hashmap_get_direct`'s call sites name
/// `java/util/concurrent/ConcurrentMap` and its fast arm only fires for an
/// exact `java/util/concurrent/ConcurrentHashMap` receiver, whose
/// `native_chm_get` is the registration a policy question is about — so that
/// gate is spelled out with [`DirectHelperGate::for_triple`] rather than
/// derived from `CONCURRENT_HASHMAP_GET_DIRECT_INFO`. Every other gate is
/// derived from its helper's own info, because for those the site class and
/// the implementing class are the same.
///
/// # Cost
///
/// `Compatible`: one `Copy` policy read and a not-taken branch — byte for byte
/// what the old predicate cost, which is why the entry point stays `#[inline]`
/// and the decision is `#[cold]`.
///
/// `JdkOnly`: [`cratonvm_native_api::NativeCallSite`]'s one relaxed `u64` load
/// and generation compare, a bounds-checked slot read for the kind, and — for
/// a `Bridge`, the common case — `jdk_only_dial_yields_to_bytecode`'s three
/// early-outs, the last of which is a memoised env bool. No string hash, no
/// class-manager lock and no hierarchy walk on any reachable steady-state
/// path. That budget is the whole point: the per-byte
/// `MessageDigest.update(byte)` loop this gate governs is what the fix is
/// accepted against, and a `resolve_id` per call would reinstate the
/// three-string hash the sibling fix has just removed from the same loop.
pub struct DirectHelperGate {
    /// The triple whose registered native the helper re-implements.
    pub triple: DirectHelperTriple,
    /// `capability::classify_native`'s answer for this gate's `(class,
    /// method)`, spelled out at construction.
    ///
    /// # Why it is a field and not a call
    ///
    /// The page's second bullet — *decide the capability question explicitly*
    /// — has two admissible answers, and this field is what makes the chosen
    /// one enforceable rather than asserted. `invoke_or_native` runs
    /// `check_native_dispatch_capability` at "dispatch site 1 of 3" and the
    /// thin helpers do not, so admitting a helper whose triple IS classified
    /// would take a gate off a path that has one today. Every gate declared
    /// below is `None`, and `every_direct_helper_gate_mirrors_classify_native`
    /// proves that against `classify_native` itself rather than against a
    /// reading of it — so the branch in [`direct_helper_gate_refuses`] is inert
    /// today and the trade is **not being made**. A future helper over a
    /// classified triple refuses instead of quietly losing the gate, which is
    /// the fail-closed direction.
    ///
    /// `classify_native` is a `match` over `class_name` with string compares;
    /// calling it per call would cost more than everything else this gate does
    /// put together. Spelled here, it is one byte load.
    pub capability: Option<cratonvm_native_api::CapabilityKind>,
    /// This gate's index into the calling VM's memo array,
    /// `JitRealm::direct_helper_gates` (assigned by `direct_helper_gates!`).
    ///
    /// nio21 wave 8 L2 (page
    /// `r21-direct-helper-gates-are-process-statics-holding-per-vm-memos-FIXED-20261009.md`):
    /// the memo itself, a `NativeCallSite` of this triple's resolution in ONE
    /// VM's `NativeMethodRegistry`, used to be a field here, i.e. per-VM
    /// state in a process static. It now lives in the VM's `JitRealm`, so the
    /// gate holds only `const` metadata and two VMs in one process (one
    /// `--real-jdk`, one `--compatible`) never share or thrash a memo.
    pub ordinal: usize,
}

/// Where a [`DirectHelperGate`] gets its triple.
///
/// Two variants and not one because of H7-1: for ten of the guarded helpers
/// the registered row is exactly the one their own decline edge dispatches to,
/// and pointing the gate at that `JitInvokeInfo` makes drift between the two
/// impossible to write. For `jit_concurrent_hashmap_get_direct` it is not —
/// the info names the `ConcurrentMap` interface and the native that runs is
/// registered on `ConcurrentHashMap` — so that one is spelled out and says
/// why.
///
/// The `JitInvokeInfo` is held by reference rather than destructured at
/// construction because these gates are `static`s: const evaluation may take a
/// reference to another `static` but may not READ through it, so `info.class_name`
/// in a `const fn` initialiser does not compile. Reading at call time costs a
/// load from a `static` that is already in cache — the gate itself was just
/// loaded — and it also means a `JitInvokeInfo` edited in place cannot leave a
/// stale copy behind in the gate.
pub enum DirectHelperTriple {
    /// The triple this helper's own decline edge dispatches to.
    Info(&'static JitInvokeInfo),
    /// The IMPLEMENTING row, when the call site names an interface.
    Implementation(&'static str, &'static str, &'static str),
}

impl DirectHelperGate {
    /// A gate for the triple this helper's own decline edge dispatches to.
    pub const fn for_info(info: &'static JitInvokeInfo, ordinal: usize) -> Self {
        Self {
            triple: DirectHelperTriple::Info(info),
            capability: None,
            ordinal,
        }
    }

    /// A gate for a triple that is NOT the one the call site names — H7-1's
    /// case. See [`DirectHelperTriple`].
    pub const fn for_triple(
        class_name: &'static str,
        method_name: &'static str,
        descriptor: &'static str,
        ordinal: usize,
    ) -> Self {
        Self {
            triple: DirectHelperTriple::Implementation(class_name, method_name, descriptor),
            capability: None,
            ordinal,
        }
    }

    /// A gate for the triple this helper's own decline edge dispatches to,
    /// additionally policed by a capability the funnel would otherwise gate
    /// at "dispatch site 1 of 3" — which a thin helper never reaches. See
    /// the `capability` field's own doc for why this is the fail-closed
    /// answer to
    /// `docs/feature-designs/real-jdk-mode.md`'s
    /// second bullet.
    pub const fn for_info_capability(
        info: &'static JitInvokeInfo,
        capability: cratonvm_native_api::CapabilityKind,
        ordinal: usize,
    ) -> Self {
        Self {
            triple: DirectHelperTriple::Info(info),
            capability: Some(capability),
            ordinal,
        }
    }

    /// The triple this gate polices.
    pub fn triple(&self) -> (&'static str, &'static str, &'static str) {
        match self.triple {
            DirectHelperTriple::Info(info) => (info.class_name, info.method_name, info.descriptor),
            DirectHelperTriple::Implementation(c, m, d) => (c, m, d),
        }
    }

    /// The class the capability field is an answer about.
    pub fn class_name(&self) -> &'static str {
        self.triple().0
    }

    /// The method the capability field is an answer about.
    pub fn method_name(&self) -> &'static str {
        self.triple().1
    }

    /// This gate's memo in `vm` (nio21 wave 8 L2): the `NativeCallSite` the
    /// VM's `JitRealm` holds at this gate's ordinal. Per VM, never shared.
    #[inline]
    pub fn site<'v>(&self, vm: &'v SharedVm) -> &'v cratonvm_native_api::NativeCallSite {
        vm.jit.direct_helper_gates.site(self.ordinal)
    }
}

direct_helper_gates! {
    /// `dbb_direct_elem_addr`'s write caller, [`jit_dbb_put_byte_direct`].
    NIO_BYTEBUFFER_PUT_BYTE_GATE = (info: NIO_BYTEBUFFER_PUT_BYTE_INFO);
    /// `dbb_direct_elem_addr`'s read caller, [`jit_dbb_get_byte_direct`].
    ///
    /// Two gates rather than one because `dbb_direct_elem_addr` is shared and
    /// its `for_write` flag is exactly the choice between two registered
    /// triples. A single gate here would police the read about `put`.
    NIO_BYTEBUFFER_GET_BYTE_GATE = (info: NIO_BYTEBUFFER_GET_BYTE_INFO);
    /// [`jit_md_update_byte_direct`] — the triple the page measures.
    MD_UPDATE_BYTE_GATE = (info: MD_UPDATE_BYTE_INFO);
    /// [`jit_preconditions_check_index_direct`].
    PRECONDITIONS_CHECK_INDEX_GATE = (info: PRECONDITIONS_CHECK_INDEX_INFO);
    /// [`jit_buffer_session_direct`]'s defensive `class_is_served` fallback
    /// only — the field-read arm above it shadows no bytecode and is
    /// deliberately ungated, as that function's own comment says.
    BUFFER_SESSION_GATE = (info: BUFFER_SESSION_INFO);
    SCOPED_MEMORY_GET_BYTE_INTERNAL_GATE = (info: SCOPED_MEMORY_GET_BYTE_INTERNAL_INFO);
    SCOPED_MEMORY_PUT_BYTE_INTERNAL_GATE = (info: SCOPED_MEMORY_PUT_BYTE_INTERNAL_INFO);
    SCOPED_MEMORY_GET_INT_GATE = (info: SCOPED_MEMORY_GET_INT_INFO);
    SCOPED_MEMORY_GET_SHORT_GATE = (info: SCOPED_MEMORY_GET_SHORT_INFO);
    SCOPED_MEMORY_GET_LONG_GATE = (info: SCOPED_MEMORY_GET_LONG_INFO);
    SCOPED_MEMORY_GET_CHAR_GATE = (info: SCOPED_MEMORY_GET_CHAR_INFO);
    SCOPED_MEMORY_PUT_INT_GATE = (info: SCOPED_MEMORY_PUT_INT_INFO);
    SCOPED_MEMORY_PUT_SHORT_GATE = (info: SCOPED_MEMORY_PUT_SHORT_INFO);
    SCOPED_MEMORY_PUT_LONG_GATE = (info: SCOPED_MEMORY_PUT_LONG_INFO);
    SCOPED_MEMORY_PUT_CHAR_GATE = (info: SCOPED_MEMORY_PUT_CHAR_INFO);
    SCOPED_MEMORY_GET_INT_INTERNAL_GATE = (info: SCOPED_MEMORY_GET_INT_INTERNAL_INFO);
    SCOPED_MEMORY_GET_SHORT_INTERNAL_GATE = (info: SCOPED_MEMORY_GET_SHORT_INTERNAL_INFO);
    SCOPED_MEMORY_GET_LONG_INTERNAL_GATE = (info: SCOPED_MEMORY_GET_LONG_INTERNAL_INFO);
    SCOPED_MEMORY_GET_CHAR_INTERNAL_GATE = (info: SCOPED_MEMORY_GET_CHAR_INTERNAL_INFO);
    SCOPED_MEMORY_PUT_INT_INTERNAL_GATE = (info: SCOPED_MEMORY_PUT_INT_INTERNAL_INFO);
    SCOPED_MEMORY_PUT_SHORT_INTERNAL_GATE = (info: SCOPED_MEMORY_PUT_SHORT_INTERNAL_INFO);
    SCOPED_MEMORY_PUT_LONG_INTERNAL_GATE = (info: SCOPED_MEMORY_PUT_LONG_INTERNAL_INFO);
    SCOPED_MEMORY_PUT_CHAR_INTERNAL_GATE = (info: SCOPED_MEMORY_PUT_CHAR_INTERNAL_INFO);
    /// [`jit_thread_current_thread_direct`] — the helper `H12-1` MEASURED the
    /// OSR door binding under `--real-jdk` (298 000 calls, `OSR 1`).
    THREAD_CURRENT_THREAD_GATE = (info: THREAD_CURRENT_THREAD_INFO);
    /// [`jit_hashmap_get_direct`].
    HASHMAP_GET_GATE = (info: HASHMAP_GET_DIRECT_INFO);
    /// [`jit_hashmap_put_direct`] — the one mutation in the population.
    HASHMAP_PUT_GATE = (info: HASHMAP_PUT_DIRECT_INFO);
    /// [`jit_concurrent_hashmap_get_direct`], spelled out rather than derived
    /// from `CONCURRENT_HASHMAP_GET_DIRECT_INFO`: that info names the
    /// `java/util/concurrent/ConcurrentMap` INTERFACE the call site resolves
    /// against, and H7-1's finding is that the policy question belongs to the
    /// implementing row whose `native_chm_get` actually runs. The fast arm
    /// screens for an exact `ConcurrentHashMap` receiver, so this is also the
    /// receiver's own class and not an approximation of it.
    CONCURRENT_HASHMAP_GET_GATE = (triple:
        "java/util/concurrent/ConcurrentHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;");
    /// [`jit_unsafe_get_byte_direct`] — capability-classified:
    /// `classify_native` maps the whole `jdk/internal/misc/Unsafe` class to
    /// `RawMemory`, so the tail's `check_native_dispatch_capability` is not a
    /// not-taken branch here the way it is for every gate above.
    UNSAFE_GET_BYTE_GATE = (info: UNSAFE_GET_BYTE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_GET_SHORT_GATE = (info: UNSAFE_GET_SHORT_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_GET_INT_GATE = (info: UNSAFE_GET_INT_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_GET_LONG_GATE = (info: UNSAFE_GET_LONG_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    /// [`jit_unsafe_get_reference_direct`] / [`jit_unsafe_get_reference_volatile_direct`]
    /// (SBR round 2, wave 8, lane `tlocal`): the reference-array read, under the
    /// same class-wide `RawMemory` classification as the scalar accessors.
    UNSAFE_GET_REFERENCE_GATE = (info: UNSAFE_GET_REFERENCE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_GET_REFERENCE_VOLATILE_GATE = (info: UNSAFE_GET_REFERENCE_VOLATILE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_BYTE_GATE = (info: UNSAFE_PUT_BYTE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_SHORT_GATE = (info: UNSAFE_PUT_SHORT_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_INT_GATE = (info: UNSAFE_PUT_INT_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_LONG_GATE = (info: UNSAFE_PUT_LONG_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    /// NIO19-2: no helper; asked only at compile time by
    /// [`unsafe_inline_admitted_mask`] (see [`UNSAFE_GET_CHAR_INFO`]).
    UNSAFE_GET_CHAR_GATE = (info: UNSAFE_GET_CHAR_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_CHAR_GATE = (info: UNSAFE_PUT_CHAR_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    /// [`jit_unsafe_cas_int_direct`] / [`jit_unsafe_cas_long_direct`] -- the
    /// same class-wide `RawMemory` classification as the eight above.
    UNSAFE_CAS_INT_GATE = (info: UNSAFE_CAS_INT_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_CAS_LONG_GATE = (info: UNSAFE_CAS_LONG_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    /// nio21 wave 3, lane L4: the volatile `Unsafe` rows, `RawMemory` like
    /// every `jdk/internal/misc/Unsafe` gate above.
    UNSAFE_GET_INT_VOLATILE_GATE = (info: UNSAFE_GET_INT_VOLATILE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_GET_LONG_VOLATILE_GATE = (info: UNSAFE_GET_LONG_VOLATILE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_INT_VOLATILE_GATE = (info: UNSAFE_PUT_INT_VOLATILE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_LONG_VOLATILE_GATE = (info: UNSAFE_PUT_LONG_VOLATILE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PUT_REFERENCE_VOLATILE_GATE = (info: UNSAFE_PUT_REFERENCE_VOLATILE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_CAS_REFERENCE_GATE = (info: UNSAFE_CAS_REFERENCE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_FULL_FENCE_GATE = (info: UNSAFE_FULL_FENCE_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    /// [`jit_reference_refers_to0_direct`] / its `PhantomReference` twin and
    /// [`jit_thread_current_carrier_thread_direct`]: unclassified triples.
    REFERENCE_REFERS_TO0_GATE = (info: REFERENCE_REFERS_TO0_INFO);
    PHANTOM_REFERENCE_REFERS_TO0_GATE = (info: PHANTOM_REFERENCE_REFERS_TO0_INFO);
    REFERENCE_GET0_GATE = (info: REFERENCE_GET0_INFO);
    THREAD_CURRENT_CARRIER_THREAD_GATE = (info: THREAD_CURRENT_CARRIER_THREAD_INFO);
    SYSTEM_CURRENT_TIME_MILLIS_GATE = (info: SYSTEM_CURRENT_TIME_MILLIS_INFO);
    SYSTEM_NANO_TIME_GATE = (info: SYSTEM_NANO_TIME_INFO);
    OBJECT_NOTIFY_GATE = (info: OBJECT_NOTIFY_INFO);
    OBJECT_NOTIFY_ALL_GATE = (info: OBJECT_NOTIFY_ALL_INFO);
    UNSAFE_UNPARK_GATE = (info: UNSAFE_UNPARK_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
    UNSAFE_PARK_GATE = (info: UNSAFE_PARK_INFO, capability: cratonvm_native_api::CapabilityKind::RawMemory);
}

/// Whether the thin direct-call helper behind `gate` must decline its fast
/// path for `vm` and route to the generic dispatch helper.
///
/// The callee-side half of the Real-JDK direct-helper gate — see the module
/// block above for why it exists on this side at all, and for the measurement
/// that showed the compile-time half covering only one of three doors.
///
/// Applies **only** to helpers that re-implement a registered native.
/// `jit_integer_value_of_direct` and `jit_integer_int_value_direct` are
/// deliberately NOT guarded: `java/lang/Integer.valueOf(I)` and
/// `java/lang/Integer.intValue()` are both `intrinsic` in
/// `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, which is §1.4's
/// reviewed exception — the one kind `direct_native_helper` also admits under
/// `JdkOnly`. Guarding them here would be stricter than the contract and would
/// cost the eager-first-call door its only two binds. The `Unsafe` scalar
/// accessors and `jit_buffer_session_direct`'s field read are not guarded
/// either, for the different reason their own banners give: those methods have
/// no bytecode, so there is nothing for a VM-side body to shadow.
#[inline]
pub fn jit_direct_helper_refused(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    // A capability-classified gate is checked in EVERY mode: the funnel's
    // `check_native_dispatch_capability` runs regardless of `--real-jdk` /
    // `--compatible`, and `--capabilities` is the switch that makes it
    // matter, not the class-library policy. Only the `Unsafe` gates are
    // classified; for every other gate this is the early return it always was.
    //
    // Outside `--real-jdk` a classified gate asks the shared decision directly
    // and does NOT go through `direct_helper_gate_refuses`: that arm counts
    // into `JIT_DIRECT_HELPER_JDK_ONLY_{ADMISSIONS,REFUSALS}`, which are
    // statements about the jdk-only policy, and a process-wide `lock xadd` per
    // call is a real cost on the `Unsafe.compareAndSetInt` loop this family
    // exists for.
    if !crate::vm::dispatch_policy(vm).is_jdk_only() {
        if gate.capability.is_some() {
            return direct_helper_tail_would_refuse(vm, gate);
        }
        // nio21 wave 2 L1 (r21 CHM-row page, edit D): under the `--compatible`
        // `jdk-concurrent-hash-map` row the map is the JDK's own (`table` of
        // `Node`s), and this helper walks the segmented stand-in store. It is
        // refused OUTRIGHT rather than through the tail question: with the
        // family's triples unregistered the tail answers "refuse" anyway
        // (`None => true`), and if some registration ever came back on the
        // triple the segmented walk would still be wrong on a JDK map.
        // Defence in depth: no compile door binds this helper today.
        //
        // JIT round 20 wave 14 (lane cjavac): the same for the exact-`HashMap`
        // pair under the `jdk-hash-collections` row, where compile doors DO
        // bind them (`invokevirtual HashMap.get/put`, `invokeinterface
        // Map.get`; see `hashmap_gate_under_jdk_model`).
        return chm_gate_under_jdk_model(vm, gate) || hashmap_gate_under_jdk_model(vm, gate);
    }
    direct_helper_gate_refuses(vm, gate)
}

/// `gate` is [`HASHMAP_GET_GATE`] or [`HASHMAP_PUT_GATE`] and the VM runs the
/// JDK's `HashMap` model under `--compatible` (the `jdk-hash-collections` row,
/// default on over a real JDK). JIT round 20 wave 14, lane cjavac
/// (`docs/feature-designs/real-jdk-mode.md`).
///
/// Under the row the `HashMap` natives are not registered: the interpreter
/// runs `HashMap.put` / `get` as the JDK's bytecode, so every entry lives in
/// the map's own `table`. The two direct helpers still answered from the
/// stand-in's stores -- the Integer-key overlay, the String-node cache and
/// `native_hashmap_get_exact` / `_put` -- which the row leaves empty for a
/// JDK-built map. A compiled `invokeinterface Map.get` bound to
/// `jit_hashmap_get_direct` therefore answered `null` for keys the
/// interpreted `put` had stored: javac's `Locations.getHandler` returned no
/// handler for `CLASS_PATH`, and `ClassFinder.fillIn` died on a `null` binary
/// name (`R20JimgSmallHeap`, `R20CjavacHotLocation`, `--compatible` only;
/// `--real-jdk` refuses the helpers through `direct_helper_gate_refuses`).
/// Refused, the helper takes its own `break 'fast` route to
/// `jit_invoke_dispatch`, which runs the JDK body. Correctness, unconditional
/// (`CRATONVM_COMPAT_ALIGN=-jdk-hash-collections` restores the stand-in maps
/// and with them the helpers' fast paths).
#[inline]
pub fn hashmap_gate_under_jdk_model(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    (std::ptr::eq(gate, &HASHMAP_GET_GATE) || std::ptr::eq(gate, &HASHMAP_PUT_GATE))
        && vm
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkHashCollections)
}

/// Bind-time twin of [`hashmap_gate_under_jdk_model`] and
/// [`chm_gate_under_jdk_model`], asked by name (the compile door has a triple,
/// not a gate): is `(class, method, descriptor)` the implementing row of an
/// exact-map thin helper whose `--compatible` row is in force, so that the
/// helper's call-time gate would refuse every call? Cold: once per recognised
/// site per compile. JIT round 20 wave 14, lane cjavac (cjavac3).
pub fn compat_row_refuses_collection_bind(
    vm: &SharedVm,
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    let row = match (class, method, descriptor) {
        ("java/util/HashMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
        | (
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        ) => cratonvm_types::HotSpotAlignment::JdkHashCollections,
        (
            "java/util/concurrent/ConcurrentHashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ) => cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap,
        _ => return false,
    };
    vm.natives.native_methods.aligned(row)
}

/// `gate` is [`CONCURRENT_HASHMAP_GET_GATE`] and the VM runs the JDK's
/// `ConcurrentHashMap` model under `--compatible` (the
/// `jdk-concurrent-hash-map` row). The pointer compare comes first so every
/// other unclassified gate pays one compare.
#[inline]
pub fn chm_gate_under_jdk_model(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    std::ptr::eq(gate, &CONCURRENT_HASHMAP_GET_GATE)
        && vm
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
}

/// The `JdkOnly` (and capability-classified) arm of
/// [`jit_direct_helper_refused`].
///
/// NOT cold, whatever this used to say: `--real-jdk` is the launcher default,
/// so under it every call into a guarded helper arrives here, and the
/// `Unsafe` gates arrive here in every mode. Kept out of line so the
/// `Compatible` early return stays small; the tally is per-thread (see
/// [`JitCounterBlock`]) because this runs per call.
#[inline(never)]
pub fn direct_helper_gate_refuses(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    let refused = direct_helper_tail_would_refuse(vm, gate);
    // t1 wave 6, lane Y3: into THIS VM's tally (`JitCache::jdk_only_sinks`).
    JIT_COUNTERS.with(|b| b.bump_direct_helper(vm, refused));
    refused
}

/// Would `vm_exec::invoke_or_native` — the tail a decline routes to — refuse
/// to run `gate`'s registered native?
///
/// [`direct_helper_refusal`]'s per-call caller. Everything this adds is the
/// memo: the gate's per-VM `NativeCallSite` turns the triple hash into one relaxed
/// load and a generation compare, so the decision can be taken per call
/// instead of once per compile. The decision itself is the bind-time one,
/// unchanged — which is the property that makes a bound helper and its own
/// guard incapable of disagreeing.
///
/// `kind_of_id` is re-read on every call rather than memoised beside the id,
/// deliberately: a re-registration updates a slot IN PLACE and does not move
/// the registry generation, so a cached kind would outlive the registration it
/// describes. `revalidate_cached_native` re-reads for the same reason, and
/// `jdk_only_dial_yields_to_bytecode`'s doc names per-call-site drift as the
/// hazard this file has already paid for once.
pub fn direct_helper_tail_would_refuse(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    let (class_name, method_name, descriptor) = gate.triple();
    let registry = &vm.natives.native_methods;
    let memo_enabled = gate_decision_memo_enabled()
        && !crate::runtime::env_cache::jdk_only_enforce_shadow();
    let has_policy = registry.capabilities().is_some();
    if memo_enabled {
        let (generation, redefine_epoch) = gate_memo_key(vm);
        if let Some(capability) = gate.capability {
            if gate_policy_half_memo_enabled() && has_policy {
                if let Some(m) = GATE_DECISION_MEMO.with(|ways| {
                    ways.get(gate.ordinal)
                        .and_then(|cell| cell.get())
                        .filter(|m| {
                            m.vm == vm.vm_identity
                                && m.generation == generation
                                && m.redefine_epoch == redefine_epoch
                        })
                }) {
                    if !thin_helper_capability_admits(vm, capability, CapabilityQuestion::Call(m.id)) {
                        return true;
                    }
                    return crate::vm::dispatch_policy(vm).is_jdk_only() && m.refused;
                }
            }
        } else if gate_memo_policy_admits(false, has_policy) {
            if let Some(refused) = GATE_DECISION_MEMO.with(|ways| {
                ways.get(gate.ordinal)
                    .and_then(|cell| cell.get())
                    .filter(|m| {
                        m.vm == vm.vm_identity
                            && m.generation == generation
                            && m.redefine_epoch == redefine_epoch
                    })
                    .map(|m| m.refused)
            }) {
                return refused;
            }
        }
    }
    let id = gate
        .site(vm)
        .resolve(registry, class_name, method_name, descriptor);
    let kind = id.and_then(|id| registry.kind_of_id(id));
    // JIT round 23 wave 8 (lane W8-L2, CF-W8-1): a CLASSIFIED gate (the
    // `Unsafe` CAS / accessor family) under an installed policy -- every
    // booted VM -- never reached the memo below, so under `--real-jdk` each
    // call re-asked the resolver half (`thin_helper_tail_yields_to_bytecode`:
    // a class-manager read lock and `find_method_recursive`), which
    // `--compatible` skips. Sampled at ~15 % of `CompletableFuture`'s pending
    // row (`UniCompletion.claim`'s `compareAndSetInt`). The split asks the
    // capability half per call, exactly as before, and memoises only the
    // policy half, whose inputs are the memo key's.
    if let Some(capability) = gate.capability {
        if gate_policy_half_memo_enabled()
            && gate_decision_memo_enabled()
            && registry.capabilities().is_some()
            && !crate::runtime::env_cache::jdk_only_enforce_shadow()
        {
            return classified_gate_refuses_split(vm, gate, capability, id, kind);
        }
    }
    // The memo below holds only while the answer is a function of `(vm, gate,
    // kind)` alone: no capability policy (whose `Audit` / `Permissive` modes
    // RECORD per call) and no armed shadow dial (whose answer walks a
    // hierarchy that loading can change). `kind` itself is re-read above on
    // every call, for the reason this function's doc gives, and is part of
    // the key.
    //
    // JIT round 21 wave 4 (lane compat, COMPAT21-2): "no capability policy" was
    // spelled `registry.capabilities().is_none()`, which is false in every
    // booted VM (`vm_init` installs a Permissive set unconditionally), so the
    // memo never engaged outside unit tests. Only a CLASSIFIED gate's answer
    // reads the policy; an unclassified one memoises whatever is installed.
    // The key also carries the registry generation and this VM's redefine
    // epoch, because COMPAT21-1's resolver term reads the loaded declaration
    // (`gate_memo_key`); `CRATONVM_JIT_GATE_DECISION_MEMO=0` is the kill switch.
    let memo = gate_decision_memo_enabled()
        && gate_memo_policy_admits(gate.capability.is_some(), registry.capabilities().is_some())
        && !crate::runtime::env_cache::jdk_only_enforce_shadow();
    let kind_tag = match kind {
        None => 0u8,
        Some(cratonvm_native_api::NativeKind::Intrinsic) => 1,
        Some(cratonvm_native_api::NativeKind::Bridge) => 2,
        Some(cratonvm_native_api::NativeKind::SyntheticStub) => 3,
    };
    let (generation, redefine_epoch) = if memo {
        gate_memo_key(vm)
    } else {
        (0, 0)
    };
    if memo {
        if let Some(refused) = GATE_DECISION_MEMO.with(|ways| {
            ways.get(gate.ordinal)
                .and_then(|cell| cell.get())
                .filter(|m| {
                    m.vm == vm.vm_identity
                        && m.kind == kind_tag
                        && m.generation == generation
                        && m.redefine_epoch == redefine_epoch
                })
                .map(|m| m.refused)
        }) {
            return refused;
        }
    }
    let refused = direct_helper_refusal(
        vm,
        class_name,
        method_name,
        descriptor,
        gate.capability
            .map(|kind| (kind, CapabilityQuestion::Call(id))),
        kind,
    );
    // COMPAT21-1's term answers "admit" for a class that is not loaded yet and
    // may answer "refuse" once it is, so such an answer is not cached; every
    // other input is fixed for the key.
    if memo && thin_helper_answer_settled(vm, class_name, method_name, descriptor, kind) {
        GATE_DECISION_MEMO.with(|ways| {
            if let Some(cell) = ways.get(gate.ordinal) {
                cell.set(Some(GateDecisionMemo {
                    vm: vm.vm_identity,
                    id,
                    kind: kind_tag,
                    generation,
                    redefine_epoch,
                    refused,
                }));
            }
        });
    }
    refused
}

/// COMPAT21-2: may [`direct_helper_tail_would_refuse`] memoise a gate's answer
/// under the installed capability policy? A classified gate's answer is the
/// funnel's capability check, which RECORDS per call under `Audit` /
/// `Permissive`, so it memoises only with no policy at all (the old rule); an
/// unclassified gate never reads the policy. Split out for its unit test.
#[inline]
pub fn gate_memo_policy_admits(gate_is_classified: bool, policy_installed: bool) -> bool {
    !gate_is_classified || !policy_installed
}

/// `CRATONVM_JIT_GATE_POLICY_HALF_MEMO` (JIT round 23 wave 8, lane W8-L2,
/// CF-W8-1; perf, default ON, `=0` restores the per-call resolver question):
/// [`classified_gate_refuses_split`]. Read once per process, as its sibling
/// [`gate_decision_memo_enabled`] is.
pub fn gate_policy_half_memo_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// [`direct_helper_tail_would_refuse`] for a capability-classified gate under
/// an installed policy, split in its two halves.
///
/// The capability half is asked on EVERY call, through the funnel's own
/// function, so `Audit` / `Permissive` record exactly as before and `Enforce`
/// can still deny. Only past it, and only under `--real-jdk` (the
/// `--compatible` answer is "admit", as in [`direct_helper_refusal`]), is the
/// policy half -- [`direct_helper_refusal`] with no capability question --
/// served from the gate's memo cell, under the same key and the same
/// settledness rule an unclassified gate's whole answer uses. The cell is
/// never filled for this gate by the unsplit path while a policy is installed
/// ([`gate_memo_policy_admits`]), and without one the two answers coincide
/// (an absent policy admits every capability question).
pub fn classified_gate_refuses_split(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    capability: cratonvm_native_api::CapabilityKind,
    id: Option<cratonvm_native_api::NativeMethodId>,
    kind: Option<cratonvm_native_api::NativeKind>,
) -> bool {
    if !thin_helper_capability_admits(vm, capability, CapabilityQuestion::Call(id)) {
        return true;
    }
    if !crate::vm::dispatch_policy(vm).is_jdk_only() {
        return false;
    }
    let (class_name, method_name, descriptor) = gate.triple();
    let kind_tag = match kind {
        None => 0u8,
        Some(cratonvm_native_api::NativeKind::Intrinsic) => 1,
        Some(cratonvm_native_api::NativeKind::Bridge) => 2,
        Some(cratonvm_native_api::NativeKind::SyntheticStub) => 3,
    };
    let (generation, redefine_epoch) = gate_memo_key(vm);
    if let Some(refused) = GATE_DECISION_MEMO.with(|ways| {
        ways.get(gate.ordinal)
            .and_then(|cell| cell.get())
            .filter(|m| {
                m.vm == vm.vm_identity
                    && m.kind == kind_tag
                    && m.generation == generation
                    && m.redefine_epoch == redefine_epoch
            })
            .map(|m| m.refused)
    }) {
        return refused;
    }
    let refused = direct_helper_refusal(vm, class_name, method_name, descriptor, None, kind);
    if thin_helper_answer_settled(vm, class_name, method_name, descriptor, kind) {
        GATE_DECISION_MEMO.with(|ways| {
            if let Some(cell) = ways.get(gate.ordinal) {
                cell.set(Some(GateDecisionMemo {
                    vm: vm.vm_identity,
                    id,
                    kind: kind_tag,
                    generation,
                    redefine_epoch,
                    refused,
                }));
            }
        });
    }
    refused
}

/// COMPAT21-2: the epoch half of the gate memo's key -- `vm`'s native registry
/// generation (moves when a new triple is registered) and its JIT redefine
/// epoch (moves when a class is redefined, `JitCache::redefine_epoch`). Two
/// relaxed/acquire `u32` loads.
#[inline]
pub fn gate_memo_key(vm: &SharedVm) -> (u32, u32) {
    (
        vm.natives.native_methods.mutation_epoch(),
        vm.jit.jit_cache.redefine_epoch(),
    )
}

/// COMPAT21-1/2: is a gate answer for `kind` on the triple final for the memo
/// key? Always for kinds the resolver term never asks about
/// (`thin_helper_resolver_yields`). For the kinds it does ask about, only once
/// the declaration the term reads is definite: `class_name` loaded, and the
/// method it resolves to either absent, `ACC_NATIVE`, `ACC_ABSTRACT` or
/// carrying a decoded `Code` -- a concrete method whose `Code` is not decoded
/// yet reads as "no bytecode" now and "bytecode" later, and neither a class
/// load nor a decode is an epoch the key carries. One recursive class-manager
/// read and one hierarchy lookup, on a memo miss only.
pub fn thin_helper_answer_settled(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: Option<cratonvm_native_api::NativeKind>,
) -> bool {
    let asks = match kind {
        None | Some(cratonvm_native_api::NativeKind::Intrinsic) => {
            // The pure-bytecode arm of the term reads the class too.
            kind.is_some()
                && crate::vm::dispatch_policy(vm).is_jdk_only()
                && cratonvm_native_api::unretire::pure_bytecode()
        }
        Some(cratonvm_native_api::NativeKind::Bridge)
        | Some(cratonvm_native_api::NativeKind::SyntheticStub) => true,
    };
    if !asks || !thin_helper_asks_resolver_enabled(vm) {
        return true;
    }
    let cm = vm.classes.class_manager.read_recursive();
    let Some(cid) = cm.get_loaded_class_id(class_name) else {
        return false;
    };
    // Through the VM's `MemberResolver` (the guard's rule for every hierarchy
    // lookup in this file): this VM's link resolver, its lookup semantics.
    let resolver = crate::runtime::resolve::MemberResolver::new(vm);
    let found = match resolver.declared_method(&cm, resolver.scope(cid), method_name, descriptor) {
        Ok(found) => found,
        // Nothing declares it from the class: final until a redefinition,
        // which the memo key's redefine epoch carries.
        Err(crate::runtime::resolve::ResolveError::NoSuchMethod { .. }) => return true,
        Err(_) => return false,
    };
    let Ok((declaring_id, index)) = resolver.adopt(found) else {
        return false;
    };
    cm.class_store()
        .get(declaring_id)
        .and_then(|declaring| declaring.methods.get(index as usize))
        .is_some_and(|m| m.is_native() || m.is_abstract() || m.code().is_some())
}

/// One gate's last call-time answer on this thread (netty compression wall,
/// 2026-10-14; `CRATONVM_JIT_GATE_DECISION_MEMO`): `vm` and the registry
/// `kind` read on that call, and the verdict [`direct_helper_refusal`] gave.
/// Per thread and keyed by the VM's identity, like [`DirectReceiverMemo`], so
/// no per-VM answer lives in a process static (the rule the gates' own
/// `NativeCallSite` memos moved to `JitRealm` for). The scoped
/// `ScopedMemoryAccess` byte helpers ask this on every netty `ByteBuf` byte,
/// and the full question -- the bytecode terms, the dial, the capability
/// funnel -- was ~4 % of `FastLzIntegrationTest#testHugeDecompress`.
#[derive(Clone, Copy)]
pub struct GateDecisionMemo {
    pub vm: usize,
    pub id: Option<cratonvm_native_api::NativeMethodId>,
    pub kind: u8,
    /// COMPAT21-2: `gate_memo_key`'s registry generation at the fill.
    pub generation: u32,
    /// COMPAT21-2: `gate_memo_key`'s redefine epoch at the fill.
    pub redefine_epoch: u32,
    pub refused: bool,
}

thread_local! {
    pub static GATE_DECISION_MEMO: [Cell<Option<GateDecisionMemo>>; DIRECT_HELPER_GATE_COUNT] =
        const { [const { Cell::new(None) }; DIRECT_HELPER_GATE_COUNT] };
}

/// `CRATONVM_JIT_GATE_DECISION_MEMO` (default ON, `=0` asks the full
/// decision on every call). Read once per process: both arms answer the same.
pub fn gate_decision_memo_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// Which half of the thin-helper policy is asking the capability question,
/// and so which of the two answers it may give.
///
/// The funnel asks `check_native_dispatch_capability` once per DISPATCH, and
/// under `Permissive`/`Audit` that call is also the audit record. A bind is
/// not a dispatch, so the two halves cannot both simply call it:
///
/// * [`CapabilityQuestion::Call`] -- per call, from the helper itself. Asks
///   `check_native_dispatch_capability_for_slot`, the funnel's own function,
///   so the helper records and refuses exactly what the funnel would have for
///   the call it stands in for. The `NativeMethodId` is the gate's memoised
///   resolution, which is what lets the per-native scope rule apply.
/// * [`CapabilityQuestion::Bind`] -- once per recognised site per compile.
///   Side-effect free: refuse only a VM whose policy is `Enforce`, where a
///   denial is possible and binding would buy a helper that declines on
///   every call. Under `Permissive`/`Audit` nothing is ever denied, so the
///   bind is admitted and each call still asks [`Self::Call`].
#[derive(Clone, Copy)]
pub enum CapabilityQuestion {
    Bind,
    Call(Option<cratonvm_native_api::NativeMethodId>),
}

/// The capability half of [`direct_helper_refusal`]: may a thin helper stand
/// in front of a triple `classify_native` classifies as `kind`?
///
/// Until 2026-09-24 this was not asked; a classified triple was refused
/// outright, which made the eight `Unsafe` accessor helpers unreachable in
/// every mode (bound nowhere, since `direct_native_bind_admitted` refused them
/// too). That was the fail-closed reading of
/// `unsafe-accessor-helpers-bypass-the-capability-funnel-at-all-three-doors`,
/// whose §3 step 1 named the precise alternative: refuse "whenever the VM's
/// capability set could deny it". This is that alternative, and the
/// per-call half is not a re-implementation of the gate but the gate --
/// `check_native_dispatch_capability_for_slot`, exactly what
/// `try_jit_site_cached_native_dispatch` calls for the same triple.
pub fn thin_helper_capability_admits(
    vm: &SharedVm,
    kind: cratonvm_native_api::CapabilityKind,
    question: CapabilityQuestion,
) -> bool {
    match question {
        // `is_granted` records nothing; `check` would. Same `Scope::Any`
        // request the dispatch gate makes, so the same grants admit it.
        CapabilityQuestion::Bind => vm
            .natives
            .native_methods
            .capabilities()
            .map_or(true, |caps| {
                caps.mode() != cratonvm_native_api::CapabilityMode::Enforce
                    || caps.is_granted(&cratonvm_native_api::Capability::of(
                        kind,
                        cratonvm_native_api::Scope::Any,
                    ))
            }),
        CapabilityQuestion::Call(id) => {
            crate::vm::vm_exec::check_native_dispatch_capability_for_slot(vm, kind, id).is_ok()
        }
    }
}

direct_native_shadows! {
    /// `java/lang/Thread.currentThread()` — the helper `H12-1` MEASURED the
    /// OSR door binding under `--real-jdk`: 298 000 compiled calls while the
    /// MethodEntry door refused all seven sites it examined.
    ThreadCurrentThread = jit_thread_current_thread_direct,
        ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;");
    /// `java/lang/ThreadLocal.get()` (SBR round 2, wave 8, lane `tlocal`): a reviewed
    /// `Intrinsic` under `--real-jdk`, served from the real `Thread.threadLocals`
    /// table by `tl_get_fast`; every other case declines to the dispatch.
    ThreadLocalGet = jit_thread_local_get_direct,
        ("java/lang/ThreadLocal", "get", "()Ljava/lang/Object;");
    /// `java/lang/ThreadLocal.set(Object)` (SBR round 2, wave 8, lane `tlocal`): the
    /// found-key overwrite of the real table's entry; everything else declines.
    ThreadLocalSet = jit_thread_local_set_direct,
        ("java/lang/ThreadLocal", "set", "(Ljava/lang/Object;)V");
    /// `jdk/internal/util/Preconditions.checkIndex`.
    PreconditionsCheckIndex = jit_preconditions_check_index_direct,
        ("jdk/internal/util/Preconditions", "checkIndex", "(IILjava/util/function/BiFunction;)I");
    /// `java/lang/ref/Reference.reachabilityFence`.
    ReachabilityFence = jit_reachability_fence_direct,
        ("java/lang/ref/Reference", "reachabilityFence", "(Ljava/lang/Object;)V");
    /// `java/lang/Object.getClass()` (JIT round 17 wave 1, lane nio; opt-in
    /// `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT`). An `ACC_NATIVE` method, so the
    /// policy answer is "the tail would run the native" in both modes.
    ObjectGetClass = jit_object_get_class_direct,
        ("java/lang/Object", "getClass", "()Ljava/lang/Class;");
    /// `java/lang/Integer.valueOf(I)` — `intrinsic`, so §1.4's reviewed
    /// exception admits it in both modes.
    IntegerValueOf = jit_integer_value_of_direct,
        ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;");
    /// `java/lang/Integer.intValue()`.
    IntegerIntValue = jit_integer_int_value_direct,
        ("java/lang/Integer", "intValue", "()I");
    /// `java/lang/Long.valueOf(J)` — the `Long` twin of `IntegerValueOf`.
    LongValueOf = jit_long_value_of_direct,
        ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;");
    /// `java/lang/Long.longValue()` — the `Long` twin of `IntegerIntValue`.
    LongLongValue = jit_long_long_value_direct,
        ("java/lang/Long", "longValue", "()J");
    /// `java/util/HashMap.get`.
    HashMapGet = jit_hashmap_get_direct,
        ("java/util/HashMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    /// `java/util/HashMap.put` — the one mutation among the collection three.
    HashMapPut = jit_hashmap_put_direct,
        ("java/util/HashMap", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    /// `java/util/concurrent/ConcurrentHashMap.get` — the IMPLEMENTING row.
    /// See [`DirectNativeShadow::triple`] and H7-1.
    ConcurrentHashMapGet = jit_concurrent_hashmap_get_direct,
        ("java/util/concurrent/ConcurrentHashMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    /// `java/lang/StringLatin1.toLowerCase` — bound by the MethodEntry
    /// single-pass ladder only; listed so the enumeration is the whole
    /// population rather than the part two doors happen to bind.
    StringLatin1ToLowerCase = jit_string_latin1_to_lower_direct,
        ("java/lang/StringLatin1", "toLowerCase", "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;");
    /// `java/security/MessageDigest.update(B)` — the triple
    /// `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
    /// measured the callee-side half of this policy on.
    MessageDigestUpdateByte = jit_md_update_byte_direct,
        ("java/security/MessageDigest", "update", "(B)V");
    /// `java/nio/ByteBuffer.get(I)` — `dbb_direct_elem_addr`'s read caller.
    ByteBufferGetByte = jit_dbb_get_byte_direct,
        ("java/nio/ByteBuffer", "get", "(I)B");
    /// `java/nio/ByteBuffer.put(I,B)` — `dbb_direct_elem_addr`'s write caller.
    ByteBufferPutByte = jit_dbb_put_byte_direct,
        ("java/nio/ByteBuffer", "put", "(IB)Ljava/nio/ByteBuffer;");
    /// `java/nio/Buffer.session()`. The shim is registered on `Buffer` itself
    /// as well as on the ten concrete buffer classes, so the base row this
    /// names resolves; the helper's own receiver screen is what decides which
    /// receivers its fast arm answers for.
    BufferSession = jit_buffer_session_direct,
        ("java/nio/Buffer", "session", "()Ljdk/internal/foreign/MemorySessionImpl;");
    /// The sixteen `ScopedMemoryAccess` unaligned accessors, all served
    /// by `scoped_memory_{read,write}_bytes`. The class is NOT in
    /// `capability::classify_native`'s table -- `java/lang/foreign/MemorySegment`
    /// is, `jdk/internal/misc/ScopedMemoryAccess` is not -- so the capability
    /// rule is a not-taken branch for all sixteen and the funnel's gate was a
    /// guaranteed `Ok(())` for them. `no_guarded_triple_is_capability_classified_today`
    /// proves that against `classify_native` itself rather than by reading.
    ScopedMemoryGetInt = jit_scoped_memory_get_int_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I");
    ScopedMemoryGetShort = jit_scoped_memory_get_short_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getShortUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S");
    ScopedMemoryGetLong = jit_scoped_memory_get_long_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getLongUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J");
    ScopedMemoryGetChar = jit_scoped_memory_get_char_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getCharUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C");
    ScopedMemoryPutInt = jit_scoped_memory_put_int_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V");
    ScopedMemoryPutShort = jit_scoped_memory_put_short_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putShortUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V");
    ScopedMemoryPutLong = jit_scoped_memory_put_long_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putLongUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V");
    ScopedMemoryPutChar = jit_scoped_memory_put_char_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putCharUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V");
    ScopedMemoryGetByteInternal = jit_scoped_memory_get_byte_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B");
    ScopedMemoryPutByteInternal = jit_scoped_memory_put_byte_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V");
    ScopedMemoryGetIntInternal = jit_scoped_memory_get_int_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getIntUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I");
    ScopedMemoryGetShortInternal = jit_scoped_memory_get_short_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getShortUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S");
    ScopedMemoryGetLongInternal = jit_scoped_memory_get_long_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getLongUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J");
    ScopedMemoryGetCharInternal = jit_scoped_memory_get_char_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "getCharUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C");
    ScopedMemoryPutIntInternal = jit_scoped_memory_put_int_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putIntUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V");
    ScopedMemoryPutShortInternal = jit_scoped_memory_put_short_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putShortUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V");
    ScopedMemoryPutLongInternal = jit_scoped_memory_put_long_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putLongUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V");
    ScopedMemoryPutCharInternal = jit_scoped_memory_put_char_internal_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "putCharUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V");
    /// The eight plain `jdk/internal/misc/Unsafe` scalar accessors, served
    /// by [`unsafe_accessor_read_bytes`]/[`unsafe_accessor_write_bytes`].
    /// Unlike every shadow above, `classify_native` maps the whole
    /// `jdk/internal/misc/Unsafe` class to `RawMemory`, so
    /// `no_guarded_triple_is_capability_classified_today` names these eight
    /// explicitly rather than asserting the set is empty.
    UnsafeGetByte = jit_unsafe_get_byte_direct,
        ("jdk/internal/misc/Unsafe", "getByte", "(Ljava/lang/Object;J)B");
    UnsafeGetShort = jit_unsafe_get_short_direct,
        ("jdk/internal/misc/Unsafe", "getShort", "(Ljava/lang/Object;J)S");
    UnsafeGetInt = jit_unsafe_get_int_direct,
        ("jdk/internal/misc/Unsafe", "getInt", "(Ljava/lang/Object;J)I");
    UnsafeGetLong = jit_unsafe_get_long_direct,
        ("jdk/internal/misc/Unsafe", "getLong", "(Ljava/lang/Object;J)J");
    /// The two reference-array reads `ConcurrentHashMap.tabAt` makes (SBR round
    /// 2, wave 8, lane `tlocal`), served by `unsafe_reference_array_read`.
    UnsafeGetReference = jit_unsafe_get_reference_direct,
        ("jdk/internal/misc/Unsafe", "getReference", "(Ljava/lang/Object;J)Ljava/lang/Object;");
    UnsafeGetReferenceVolatile = jit_unsafe_get_reference_volatile_direct,
        ("jdk/internal/misc/Unsafe", "getReferenceVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;");
    UnsafePutByte = jit_unsafe_put_byte_direct,
        ("jdk/internal/misc/Unsafe", "putByte", "(Ljava/lang/Object;JB)V");
    UnsafePutShort = jit_unsafe_put_short_direct,
        ("jdk/internal/misc/Unsafe", "putShort", "(Ljava/lang/Object;JS)V");
    UnsafePutInt = jit_unsafe_put_int_direct,
        ("jdk/internal/misc/Unsafe", "putInt", "(Ljava/lang/Object;JI)V");
    UnsafePutLong = jit_unsafe_put_long_direct,
        ("jdk/internal/misc/Unsafe", "putLong", "(Ljava/lang/Object;JJ)V");
    /// The two `compareAndSet` forms, served by `unsafe_cas_serve`.
    UnsafeCasInt = jit_unsafe_cas_int_direct,
        ("jdk/internal/misc/Unsafe", "compareAndSetInt", "(Ljava/lang/Object;JII)Z");
    UnsafeCasLong = jit_unsafe_cas_long_direct,
        ("jdk/internal/misc/Unsafe", "compareAndSetLong", "(Ljava/lang/Object;JJJ)Z");
    /// nio21 wave 3, lane L4 (page
    /// `r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md`):
    /// the `ACC_NATIVE` rows the `HttpClient` census found on the generic
    /// native call. All `Bridge` natives with no bytecode to prefer, so the
    /// policy answer is "the tail would run the native" in both modes.
    UnsafeGetIntVolatile = jit_unsafe_get_int_volatile_direct,
        ("jdk/internal/misc/Unsafe", "getIntVolatile", "(Ljava/lang/Object;J)I");
    UnsafeGetLongVolatile = jit_unsafe_get_long_volatile_direct,
        ("jdk/internal/misc/Unsafe", "getLongVolatile", "(Ljava/lang/Object;J)J");
    UnsafePutIntVolatile = jit_unsafe_put_int_volatile_direct,
        ("jdk/internal/misc/Unsafe", "putIntVolatile", "(Ljava/lang/Object;JI)V");
    UnsafePutLongVolatile = jit_unsafe_put_long_volatile_direct,
        ("jdk/internal/misc/Unsafe", "putLongVolatile", "(Ljava/lang/Object;JJ)V");
    UnsafePutReferenceVolatile = jit_unsafe_put_reference_volatile_direct,
        ("jdk/internal/misc/Unsafe", "putReferenceVolatile", "(Ljava/lang/Object;JLjava/lang/Object;)V");
    UnsafeCasReference = jit_unsafe_cas_reference_direct,
        ("jdk/internal/misc/Unsafe", "compareAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z");
    UnsafeFullFence = jit_unsafe_full_fence_direct,
        ("jdk/internal/misc/Unsafe", "fullFence", "()V");
    ReferenceRefersTo0 = jit_reference_refers_to0_direct,
        ("java/lang/ref/Reference", "refersTo0", "(Ljava/lang/Object;)Z");
    PhantomReferenceRefersTo0 = jit_phantom_reference_refers_to0_direct,
        ("java/lang/ref/PhantomReference", "refersTo0", "(Ljava/lang/Object;)Z");
    ReferenceGet0 = jit_reference_get0_direct,
        ("java/lang/ref/Reference", "get0", "()Ljava/lang/Object;");
    ThreadCurrentCarrierThread = jit_thread_current_carrier_thread_direct,
        ("java/lang/Thread", "currentCarrierThread", "()Ljava/lang/Thread;");
    ScopedMemoryCopyMemoryInternal = jit_scoped_memory_copy_memory_direct,
        ("jdk/internal/misc/ScopedMemoryAccess", "copyMemoryInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V");
    UnsafeCopyMemory0 = jit_unsafe_copy_memory_direct,
        ("jdk/internal/misc/Unsafe", "copyMemory0", "(Ljava/lang/Object;JLjava/lang/Object;JJ)V");
    SystemCurrentTimeMillis = jit_system_current_time_millis_direct,
        ("java/lang/System", "currentTimeMillis", "()J");
    SystemNanoTime = jit_system_nano_time_direct,
        ("java/lang/System", "nanoTime", "()J");
    ObjectNotify = jit_object_notify_direct,
        ("java/lang/Object", "notify", "()V");
    ObjectNotifyAll = jit_object_notify_all_direct,
        ("java/lang/Object", "notifyAll", "()V");
    UnsafeUnpark = jit_unsafe_unpark_direct,
        ("jdk/internal/misc/Unsafe", "unpark", "(Ljava/lang/Object;)V");
    UnsafePark = jit_unsafe_park_direct,
        ("jdk/internal/misc/Unsafe", "park", "(ZJ)V");
}

impl DirectNativeShadow {
    /// The shadow a recognised `ScopedMemoryAccess` site binds.
    ///
    /// `cratonvm_jit::ScopedMemoryOp` is the jit crate's recognition of the
    /// call site and this is the VM crate's policy identity for the same
    /// sixteen methods, so the two have to agree about which method each op
    /// IS. They are written in different crates -- the jit crate cannot name
    /// a VM symbol, which is why the two enums exist at all -- so
    /// `every_scoped_memory_op_maps_to_the_shadow_that_spells_its_triple`
    /// round-trips this map through `is_scoped_memory_op` rather than trusting
    /// that two hand-written orderings stayed aligned.
    pub fn for_scoped_memory_op(op: cratonvm_jit::ScopedMemoryOp) -> DirectNativeShadow {
        match op {
            cratonvm_jit::ScopedMemoryOp::GetByteInternal => {
                DirectNativeShadow::ScopedMemoryGetByteInternal
            }
            cratonvm_jit::ScopedMemoryOp::PutByteInternal => {
                DirectNativeShadow::ScopedMemoryPutByteInternal
            }
            cratonvm_jit::ScopedMemoryOp::GetIntUnaligned => DirectNativeShadow::ScopedMemoryGetInt,
            cratonvm_jit::ScopedMemoryOp::GetShortUnaligned => {
                DirectNativeShadow::ScopedMemoryGetShort
            }
            cratonvm_jit::ScopedMemoryOp::GetLongUnaligned => {
                DirectNativeShadow::ScopedMemoryGetLong
            }
            cratonvm_jit::ScopedMemoryOp::GetCharUnaligned => {
                DirectNativeShadow::ScopedMemoryGetChar
            }
            cratonvm_jit::ScopedMemoryOp::PutIntUnaligned => DirectNativeShadow::ScopedMemoryPutInt,
            cratonvm_jit::ScopedMemoryOp::PutShortUnaligned => {
                DirectNativeShadow::ScopedMemoryPutShort
            }
            cratonvm_jit::ScopedMemoryOp::PutLongUnaligned => {
                DirectNativeShadow::ScopedMemoryPutLong
            }
            cratonvm_jit::ScopedMemoryOp::PutCharUnaligned => {
                DirectNativeShadow::ScopedMemoryPutChar
            }
            cratonvm_jit::ScopedMemoryOp::GetIntUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryGetIntInternal
            }
            cratonvm_jit::ScopedMemoryOp::GetShortUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryGetShortInternal
            }
            cratonvm_jit::ScopedMemoryOp::GetLongUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryGetLongInternal
            }
            cratonvm_jit::ScopedMemoryOp::GetCharUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryGetCharInternal
            }
            cratonvm_jit::ScopedMemoryOp::PutIntUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryPutIntInternal
            }
            cratonvm_jit::ScopedMemoryOp::PutShortUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryPutShortInternal
            }
            cratonvm_jit::ScopedMemoryOp::PutLongUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryPutLongInternal
            }
            cratonvm_jit::ScopedMemoryOp::PutCharUnalignedInternal => {
                DirectNativeShadow::ScopedMemoryPutCharInternal
            }
            cratonvm_jit::ScopedMemoryOp::CopyMemoryInternal => {
                DirectNativeShadow::ScopedMemoryCopyMemoryInternal
            }
        }
    }

    /// The shadow a recognised `Unsafe` scalar-accessor site binds. The jit
    /// crate's [`cratonvm_jit::UnsafeAccessorOp`] and this map are the same
    /// split `for_scoped_memory_op` documents, for the same reason: the jit
    /// crate cannot name a VM symbol.
    pub fn for_unsafe_accessor_op(op: cratonvm_jit::UnsafeAccessorOp) -> DirectNativeShadow {
        match op {
            cratonvm_jit::UnsafeAccessorOp::GetByte => DirectNativeShadow::UnsafeGetByte,
            cratonvm_jit::UnsafeAccessorOp::GetShort => DirectNativeShadow::UnsafeGetShort,
            cratonvm_jit::UnsafeAccessorOp::GetInt => DirectNativeShadow::UnsafeGetInt,
            cratonvm_jit::UnsafeAccessorOp::GetLong => DirectNativeShadow::UnsafeGetLong,
            cratonvm_jit::UnsafeAccessorOp::GetReference => DirectNativeShadow::UnsafeGetReference,
            cratonvm_jit::UnsafeAccessorOp::GetReferenceVolatile => {
                DirectNativeShadow::UnsafeGetReferenceVolatile
            }
            cratonvm_jit::UnsafeAccessorOp::PutByte => DirectNativeShadow::UnsafePutByte,
            cratonvm_jit::UnsafeAccessorOp::PutShort => DirectNativeShadow::UnsafePutShort,
            cratonvm_jit::UnsafeAccessorOp::PutInt => DirectNativeShadow::UnsafePutInt,
            cratonvm_jit::UnsafeAccessorOp::PutLong => DirectNativeShadow::UnsafePutLong,
            cratonvm_jit::UnsafeAccessorOp::CasInt => DirectNativeShadow::UnsafeCasInt,
            cratonvm_jit::UnsafeAccessorOp::CasLong => DirectNativeShadow::UnsafeCasLong,
            cratonvm_jit::UnsafeAccessorOp::GetIntVolatile => {
                DirectNativeShadow::UnsafeGetIntVolatile
            }
            cratonvm_jit::UnsafeAccessorOp::GetLongVolatile => {
                DirectNativeShadow::UnsafeGetLongVolatile
            }
            cratonvm_jit::UnsafeAccessorOp::PutIntVolatile => {
                DirectNativeShadow::UnsafePutIntVolatile
            }
            cratonvm_jit::UnsafeAccessorOp::PutLongVolatile => {
                DirectNativeShadow::UnsafePutLongVolatile
            }
            cratonvm_jit::UnsafeAccessorOp::PutReferenceVolatile => {
                DirectNativeShadow::UnsafePutReferenceVolatile
            }
            cratonvm_jit::UnsafeAccessorOp::CasReference => DirectNativeShadow::UnsafeCasReference,
            cratonvm_jit::UnsafeAccessorOp::FullFence => DirectNativeShadow::UnsafeFullFence,
            cratonvm_jit::UnsafeAccessorOp::RefersTo0 => DirectNativeShadow::ReferenceRefersTo0,
            cratonvm_jit::UnsafeAccessorOp::PhantomRefersTo0 => {
                DirectNativeShadow::PhantomReferenceRefersTo0
            }
            cratonvm_jit::UnsafeAccessorOp::Get0 => DirectNativeShadow::ReferenceGet0,
            cratonvm_jit::UnsafeAccessorOp::CopyMemory0 => DirectNativeShadow::UnsafeCopyMemory0,
            cratonvm_jit::UnsafeAccessorOp::Unpark => DirectNativeShadow::UnsafeUnpark,
            cratonvm_jit::UnsafeAccessorOp::Park => DirectNativeShadow::UnsafePark,
        }
    }
}

/// May a compile door bind a thin direct-call helper that re-implements this
/// triple's registered native?
///
/// The bind-time half of the policy [`jit_direct_helper_refused`] enforces per
/// call, asking the same question through the same decision
/// ([`direct_helper_decision`]) so that the two cannot drift:
/// `a-serviceability-predicate-must-mirror-the-dispatch-it-guards`.
///
/// This is the predicate the VM hands `jit::try_compile` as
/// `CompileRequest::direct_bind_admission`, and it is the canonical copy of
/// the closure `jit_bridge.rs` used to write out three times.
///
/// # It was `is_reviewed_intrinsic_native` until 2026-09-22
///
/// That version answered `true` only for `NativeKind::Intrinsic` and `false`
/// for every `Bridge`. It reads as the strict answer and is not one, for the
/// reason [`jit_direct_helper_jdk_only_refusals`] now states at length: a
/// refused bind leaves the site with its `invoke_info` and sends the call to
/// `vm_exec::invoke_or_native`, which runs the same `Bridge` anyway. Refusing
/// the bind bought no policy and cost a three-string registry hash per call.
///
/// Fail-closed where it matters and unchanged there: `false` for a
/// `SyntheticStub` (§1.3 — a fake may not be invoked), `false` for a triple
/// the registry has never heard of (nothing to audit), `false` when
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed (the dial exists to price
/// retirements and a helper that kept serving under it would corrupt the
/// measurement), and `false` for a capability-classified triple (the funnel
/// gates those and a thin helper cannot).
///
/// Compile-time only. One registry lookup on the strict arm, per recognised
/// site, per compilation — no memo, because there is no site to hang one on
/// and a compile is orders of magnitude more expensive than a triple hash.
pub fn direct_native_bind_admitted(
    vm: &SharedVm,
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    // No capability classification is spelled for a bind-time caller, so
    // derive it. This path is cold — once per recognised site per compilation
    // — which is exactly the budget [`DirectHelperGate::capability`] does not
    // have and the reason that one is a field.
    //
    // Checked BEFORE the `--real-jdk` early return, and unconditionally of
    // its answer: a capability grant is a property of the VM, fixed at
    // startup, not of the class-library policy this early return is about —
    // see `jit_direct_helper_refused`'s mirror of this same rule. For every
    // triple guarded today `capability` is `None`, so this is a not-taken
    // branch and the early return below is unchanged for every existing
    // caller.
    let capability = cratonvm_native_api::capability::classify_native(class, method);
    if capability.is_none() && !crate::vm::dispatch_policy(vm).is_jdk_only() {
        // JIT round 20 wave 14 (lane cjavac, cjavac3): the bind-time half of
        // `hashmap_gate_under_jdk_model` / `chm_gate_under_jdk_model`. Under a
        // `--compatible` row that runs the JDK's own map, the exact-map thin
        // helper would refuse on every call; the compile doors ask this
        // (`jit/src/lib.rs` `compat_collection_bind_refused`) and leave the
        // site to ordinary dispatch instead of binding it.
        return !compat_row_refuses_collection_bind(vm, class, method, descriptor)
            && !compat_bind_refuses_unregistered(vm, class, method, descriptor)
            && !compat_bind_yields_to_bytecode(vm, class, method, descriptor);
    }
    let registry = &vm.natives.native_methods;
    let kind = registry
        .resolve_id(class, method, descriptor)
        .and_then(|id| registry.kind_of_id(id));
    !direct_helper_refusal(
        vm,
        class,
        method,
        descriptor,
        capability.map(|kind| (kind, CapabilityQuestion::Bind)),
        kind,
    )
}

/// `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY` (JIT round 23 wave 1, lane L6;
/// SYNC22-3 of `jit-r22-sync-proposals.md`; HotSpot parity, default ON, `0`
/// kills it). Read once per VM at construction
/// (`SharedVm::jit_compat_bind_asks_registry`).
#[inline]
pub fn compat_bind_asks_registry_enabled(vm: &SharedVm) -> bool {
    vm.jit_compat_bind_asks_registry
}

/// The `--compatible` arm of [`direct_native_bind_admitted`] for an
/// unclassified triple: refuse a thin helper whose triple the registry holds
/// NO row for. `true` = refuse.
///
/// Such a triple was unregistered by an adopted `COMPAT_FAMILIES` row (or any
/// other `--compatible` drop: the stream surface, JDK-driven virtual threads),
/// or never registered at all. Either way `vm_exec::invoke_or_native` resolves
/// nothing and runs the JDK's bytecode, as the interpreter does; a bound helper
/// would be compiled code alone running a VM-side body in front of it (the
/// contract question of
/// `docs/feature-designs/real-jdk-mode.md`).
/// It is `--real-jdk`'s `None => refuse` arm of [`direct_helper_refusal`], and
/// nothing more: a registered row of any kind is still admitted here (pricing
/// the registered `SyntheticStub` rows is that page's option 1 / 2, not this).
///
/// Compile-time only (one registry hash per recognised site per compile). The
/// jit asks it from `direct_native_helper` in `--compatible` since this
/// change; before it, only the OSR door, the VM-side ladders and the
/// collection / scoped-memory doors asked this predicate in that mode.
/// zt t27/jd1 item 2 (`CRATONVM_JIT_COMPAT_BIND_ASKS_RESOLVER`, default ON,
/// `=0` kills it; read once per VM, `SharedVm::jit_compat_bind_asks_resolver`):
/// the `--compatible` arm of [`direct_native_bind_admitted`] for a REGISTERED
/// unclassified triple. `true` = refuse.
///
/// [`compat_bind_refuses_unregistered`] refuses a triple with no row; a row of
/// kind `Bridge` or `SyntheticStub` was still admitted, so compiled code ran the
/// thin helper's VM-side body where the interpreter and the generic tail
/// (`vm_exec::invoke_or_native`, `registered_native_yields_to_loaded_bytecode`)
/// run the loaded JDK body (`resolve_dispatch` step 3: real class bytes beat a
/// registered `Bridge` / `SyntheticStub`): `Preconditions.checkIndex`,
/// `DirectByteBuffer.get(I)B` / `put(IB)` (asked on the implementing row by
/// `direct_native_helper_for_impl`), `Reference.reachabilityFence`, the
/// `ScopedMemoryAccess` stub rows, `Buffer.session()`
/// (`docs/feature-designs/zgc-architecture.md` zt27 jd1 item 2). This asks the
/// question `--real-jdk` already asks ([`thin_helper_tail_yields_to_bytecode`]),
/// at bind time only: a refused site is never bound, so the call-time gate
/// ([`jit_direct_helper_refused`]) has nothing to disagree with, and the hot
/// `--compatible` early return there is unchanged. A refused site keeps its
/// ordinary ladder (dispatch, IC or splice of the JDK body) -- what
/// `--real-jdk`, where those stub rows are not registered at all, runs today.
/// An `Intrinsic` row is never refused (§1.4's reviewed exception).
///
/// Compile-time only: one registry hash plus, for a `Bridge` / stub row, the
/// resolver's loaded-declaration lookup, per recognised site per compile.
pub fn compat_bind_yields_to_bytecode(
    vm: &SharedVm,
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    if !vm.jit_compat_bind_asks_resolver {
        return false;
    }
    let registry = &vm.natives.native_methods;
    let Some(kind) = registry
        .resolve_id(class, method, descriptor)
        .and_then(|id| registry.kind_of_id(id))
    else {
        return false;
    };
    let yields = thin_helper_tail_yields_to_bytecode(vm, class, method, descriptor, kind);
    if yields && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] compat-direct-bind REFUSED {class}.{method}{descriptor}: the \
             registered {kind:?} yields to the loaded bytecode; the site keeps its bytecode \
             dispatch"
        );
    }
    yields
}

pub fn compat_bind_refuses_unregistered(
    vm: &SharedVm,
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    compat_bind_asks_registry_enabled(vm)
        && vm
            .natives
            .native_methods
            .resolve_id(class, method, descriptor)
            .is_none()
        && !compat_site_row_inherited_or_unknown(vm, class, method, descriptor)
}

/// Review fix-back (JIT r23 w1 L12 page
/// `r23-w1-review-compat-bind-asks-exact-site-triple-FIXED-20261021.md`): the site
/// triple has no row of its own, but does the generic tail still reach a
/// registered native from it, or can the answer not be known here? `true` =
/// do not refuse.
///
/// Most `direct_native_helper` callers name the call site's class, while a row
/// may be registered on a superclass: `vm_exec::invoke_or_native` then finds
/// it through the shared superclass walk ([`crate::runtime::interpreter::ancestor_native`],
/// the walk the dispatch doors and the JIT site cache use), so the helper
/// stands for that same native and the bind keeps. The question asked here is
/// that walk's, from the site class:
/// - the site class declares the method: a concrete (or `native`) body with no
///   row is what the tail runs, so refuse; an `abstract` one, or an interface
///   site, leaves the choice to the receiver's class, so do not refuse (the
///   per-call screens and the call-time gate decide, as before SYNC22-3);
/// - otherwise walk the superclasses: a registered native (`Native`) keeps the
///   bind, bytecode first or the chain's end refuses it.
///
/// Anything this cannot see -- the site class not loaded, the class manager
/// busy (`try_read_untracked`, non-blocking: the compile doors may already hold
/// it), an unresolvable ancestor or a redefine stop -- does not refuse, i.e.
/// keeps the pre-SYNC22-3 bind.
pub fn compat_site_row_inherited_or_unknown(
    vm: &SharedVm,
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    use crate::runtime::interpreter::{AncestorGuard, AncestorNative};
    let Some(cm) = vm.classes.class_manager.try_read_untracked() else {
        return true;
    };
    let Some(cid) = cm.get_loaded_class_id(class) else {
        return true;
    };
    let Some(site) = cm.get_class(cid) else {
        return true;
    };
    if site.is_interface() {
        return true;
    }
    if let Some(declared) = site.find_method(method, descriptor) {
        return declared
            .access_flags
            .contains(cratonvm_reader::class_access_flags::MethodAccessFlags::ABSTRACT);
    }
    !matches!(
        crate::runtime::interpreter::ancestor_native(
            vm,
            &cm,
            (cid, None),
            method,
            descriptor,
            |_, _| AncestorGuard::Look,
            |_, _, _| true,
        ),
        AncestorNative::Bytecode | AncestorNative::ChainEnd
    )
}

/// The one decision both halves of the thin-direct-helper policy make:
/// **would `vm_exec::invoke_or_native`, the tail a decline routes to, refuse
/// to run this triple's registered native?**
///
/// `true` = refuse the helper. Called from [`direct_native_bind_admitted`] at
/// compile time and from [`direct_helper_tail_would_refuse`] per call; the
/// callers differ only in where `kind` and `capability` came from, which is a
/// cost question and not a policy one.
///
/// This is `resolve_native_dispatch_wave1`'s `JdkOnly` arm, arm for arm, with
/// `compat_native_wins` fixed at `true`: reaching a thin helper's fast arm at
/// all IS that verdict, because the helper has already matched its own
/// receiver screen and the generic tail has no competing bytecode preference
/// for a `Bridge`. [`jit_fast_native_bytecode_terms`] supplies §7 step 3's
/// input in `invoke_or_native`'s own terms, which is the whole content of the
/// 2026-09-22 fix — see [`jit_direct_helper_jdk_only_refusals`] for what the
/// old unconditional refusal was actually doing.
///
/// Two places it is deliberately **stricter** than the tail, both named in the
/// page's "what would settle it":
///
/// * **Capability** — no longer stricter, the SAME. The tail runs
///   `check_native_dispatch_capability` at "dispatch site 1 of 3"; a
///   classified triple (today only the `Unsafe` gates, pinned by
///   `only_the_ten_unsafe_gates_are_capability_classified`) asks that same
///   check through [`thin_helper_capability_admits`] — per call, the funnel's
///   own function; at bind time, a side-effect-free "could this VM's policy
///   deny it". It is asked in EVERY mode, before the `--real-jdk` question,
///   because a grant is a property of the VM and not of the class-library
///   policy; a `--compatible` VM that passes it is then admitted exactly as
///   an unclassified gate is. Until 2026-09-24 a classified triple was simply
///   refused, which left every `Unsafe` helper bound nowhere.
/// * **The enforcement dial.** `CRATONVM_ENFORCE_NATIVE_SHADOW` armed
///   anywhere refuses every guarded helper, which is coarser than the
///   per-class question `jdk_only_dial_yields_to_bytecode` asks below. A gate
///   names the triple whose native runs; the dial is keyed on the RECEIVER's
///   runtime class, and for a virtual decline edge those differ
///   (`MD_UPDATE_BYTE_GATE` says `java/security/MessageDigest`, the receiver
///   is a provider's `DigestBase` subclass). The dial's own contract is that
///   *an armed FAILURE is real* — it exists to price retirements — so a
///   helper that kept serving under it would corrupt the measurement it was
///   armed to take. Resolving the receiver class per call instead would cost a
///   class-manager read lock on the loop this is accepted against, and
///   `jdk_only_enforce_shadow` is a memoised bool that is off on every run not
///   taking that measurement.
///
/// Not routed through `resolve_native_dispatch_wave1` itself, though it
/// answers the same question, and the reason is that function's side effects:
/// it is the *decision* site, so it feeds `note_dial_door`,
/// `record_native_shadows_bytecode` and `record_intrinsic_shadow_exempt`. A
/// guarded helper that admits is NOT a dispatch — the dispatch it stands in
/// for never happens — and a helper that refuses hands the call to the tail,
/// which runs those hooks itself. Asking here as well would double-count every
/// refused call in `--real-jdk-report`'s observation sinks, once for the
/// question and once for the answer.
///
/// Callers must have established `JdkOnly` first for an UNCLASSIFIED triple;
/// this does not re-test it, because both do so to reach their own cold arm.
/// A classified triple reaches here in every mode, so its arm re-tests.
pub fn direct_helper_refusal(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    capability: Option<(cratonvm_native_api::CapabilityKind, CapabilityQuestion)>,
    kind: Option<cratonvm_native_api::NativeKind>,
) -> bool {
    if let Some((capability, question)) = capability {
        if !thin_helper_capability_admits(vm, capability, question) {
            return true;
        }
        // The capability question is the only reason a `--compatible` VM
        // reaches this function, and it has been answered. Everything below
        // is the `--real-jdk` policy, which an unclassified gate never asks
        // outside that mode (`jit_direct_helper_refused`'s early return).
        if !crate::vm::dispatch_policy(vm).is_jdk_only() {
            return false;
        }
    }
    if crate::runtime::env_cache::jdk_only_enforce_shadow() {
        return true;
    }
    match kind {
        // No registration, or a handle this registry does not know: the tail
        // resolves nothing and runs the real bytes, so a VM-side
        // re-implementation must not stand in front of them.
        // `admit_jit_fast_native`'s rule, for the reason it gives — a body the
        // registry has never heard of cannot be policy-audited.
        None => true,
        // §1.3 — a fake may not be invoked under `--real-jdk`. The tail turns
        // this into `VmError::JdkOnly`; declining hands it the call so that it
        // does, rather than serving the fabrication from compiled code.
        Some(cratonvm_native_api::NativeKind::SyntheticStub) => true,
        // §1.4's reviewed exception for an `Intrinsic`, and §7 step 3 for a
        // `Bridge`, both in `invoke_or_native`'s own terms.
        //
        // JIT round 21 wave 4 (lane compat, COMPAT21-1): those terms are the
        // tail's as of 2026-09-22. Since `0dac905fcf` (2026-10-08) the tail
        // asks `registered_native_yields_to_loaded_bytecode` -- i.e.
        // `resolve_dispatch` on the loaded declaration -- so a `Bridge` in
        // front of concrete bytecode YIELDS there, and this arm used to admit
        // it: compiled code ran the VM-side body while the interpreter and the
        // tail ran the JDK's bytes. Asked first, so the stale mirror can only
        // add refusals (the dial), never admit what the resolver yields.
        Some(kind) => {
            thin_helper_tail_yields_to_bytecode(vm, class_name, method_name, descriptor, kind)
                || jit_fast_native_bytecode_terms(vm, class_name, method_name, descriptor, kind).1
        }
    }
}

/// `CRATONVM_JIT_THIN_HELPER_ASKS_RESOLVER` (JIT round 21 wave 4, lane compat,
/// COMPAT21-1; correctness, default ON, `=0` restores the 2026-09-22 mirror
/// alone): [`direct_helper_refusal`] asks the resolver the generic tail asks.
/// Read once per VM at construction (`SharedVm::jit_thin_helper_asks_resolver`),
/// so a VM built under a per-thread override keeps its own answer.
#[inline]
pub fn thin_helper_asks_resolver_enabled(vm: &SharedVm) -> bool {
    vm.jit_thin_helper_asks_resolver
}

/// Would `vm_exec::invoke_or_native` -- the tail a thin helper's decline
/// routes to -- yield `(class_name, method_name, descriptor)`'s registered
/// native of `kind` to the loaded declaration's bytecode? Behind
/// [`thin_helper_asks_resolver_enabled`]; the decision is
/// [`thin_helper_resolver_yields`].
pub fn thin_helper_tail_yields_to_bytecode(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    thin_helper_asks_resolver_enabled(vm)
        && thin_helper_resolver_yields(vm, class_name, method_name, descriptor, kind)
}

/// The tail's own question, `crate::vm::registered_native_yields_to_loaded_bytecode`
/// (`resolve_dispatch` with the registered kind against the method the loaded
/// class resolves), asked only where it can answer "yield": a `Bridge` or
/// `SyntheticStub` (§7 step 3), and an `Intrinsic` only under the opt-in
/// `CRATONVM_PURE_BYTECODE` probe (`resolve_dispatch` step 2's `pure` term).
/// Every other `Intrinsic` -- the hot thin helpers -- pays one `match`, no hash.
///
/// A class that is not loaded answers `false` (admit), as the tail does; a
/// thin helper is only ever CALLED with a receiver of, or a static call into,
/// a loaded class, so the call-time half never sees that case.
pub fn thin_helper_resolver_yields(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    let can_yield = match kind {
        cratonvm_native_api::NativeKind::Bridge
        | cratonvm_native_api::NativeKind::SyntheticStub => true,
        cratonvm_native_api::NativeKind::Intrinsic => {
            crate::vm::dispatch_policy(vm).is_jdk_only()
                && cratonvm_native_api::unretire::pure_bytecode_for(class_name, method_name)
        }
    };
    if !can_yield {
        return false;
    }
    let Some(callback) = vm
        .natives
        .native_methods
        .find(class_name, method_name, descriptor)
    else {
        return false;
    };
    crate::vm::registered_native_yields_to_loaded_bytecode(
        vm,
        class_name,
        method_name,
        descriptor,
        callback,
        kind,
    )
}

/// The one sanctioned way for a compile door to obtain a thin direct-call
/// helper's entry address.
///
/// `Some(addr)` — bind it. `None` — **fall through**, do not `continue`: the
/// site must still get its `invoke_info` or the backend reads the pc as a raw
/// self-call. See the module block above for the backend's own statement of
/// that rule; it is the reason this returns an `Option` at the bind site
/// instead of the plan being filtered later.
///
/// The registry kind is asked about the triple whose native actually runs.
/// Under `JdkOnly` a refusal is recorded through
/// `cratonvm_jit::record_jdk_only_direct_native_refusal`, i.e. into the same
/// counter and the same `--real-jdk-report` violation list the MethodEntry
/// door's refusals land in.
///
/// # Both policies, and why two
///
/// `admission` carries the policy the DOOR declared
/// (`CompileAdmission::declare_direct_call_policy`), and
/// `CompileAdmission::admits_direct_bind` is where the rule is written once,
/// in the crate that owns the gate. `vm` carries the policy the VM is actually
/// executing under. Both must admit.
///
/// They are the same expression at both doors today -- each declares
/// `from_jdk_only(dispatch_policy(shared).is_jdk_only())` a few hundred lines
/// above the ladder -- so the second test never fires, and it is here anyway
/// because the two answer different questions. The declaration is a door's
/// statement ABOUT ITSELF, and `H12-1`'s whole finding is that a door's own
/// account of what it does is the thing that was wrong: `--real-jdk` and
/// `--fast-boot` measured an identical bind profile while one of them was
/// supposed to be refusing. A declaration that drifts from the VM it is
/// compiling for must not be able to widen this gate, so the VM's own policy
/// is asked directly and not taken on the door's word.
///
/// A door that declares NOTHING is admitted by `admits_direct_bind` -- the
/// safe default, since refusing for a door that has not been taught to fall
/// through would strand the pc with no metadata -- and its rows are counted by
/// `undeclared_direct_bind_rows`. Such a door is still held to the VM's policy
/// here, which is the difference between "loud" and "open".
///
/// Compile-time only — never called from emitted code. One registry lookup on
/// the strict arm, per recognised site, per compilation.
pub fn admit_direct_native_entry(
    vm: &SharedVm,
    admission: &cratonvm_jit::compile_gate::CompileAdmission,
    shadow: DirectNativeShadow,
) -> Option<usize> {
    let (class, method, descriptor) = shadow.triple();
    // Evaluated ONCE: it is the registry lookup, and both tests want the same
    // answer to the same question.
    let tail_would_run_the_native = direct_native_bind_admitted(vm, class, method, descriptor);
    // The mode is read once (JIT r23 w2 L6: the bare-gate census counts each
    // read; the three questions below are one policy decision).
    let policy = cratonvm_jit::compile_gate::DirectCallPolicy::from_jdk_only(
        crate::vm::dispatch_policy(vm).is_jdk_only(),
    );
    let compatible = matches!(policy, cratonvm_jit::compile_gate::DirectCallPolicy::Compatible);
    // JIT round 20 wave 14 (lane cjavac3): `DirectCallPolicy::Compatible`
    // admits every shadow bind, so the OSR door asks the `--compatible`
    // collection rows itself -- the same question the method-entry doors ask
    // through `direct_native_bind_admitted` (`compat_collection_bind_refused`).
    // SYNC22-3 (JIT r23 w1 L6, `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY`): and
    // the `--compatible` refusal of a triple the registry holds no row for
    // (nor an inherited one), which `DirectCallPolicy::Compatible` would
    // otherwise admit. Neither is a `--real-jdk` violation, so neither is
    // recorded as one.
    if compatible
        && (compat_row_refuses_collection_bind(vm, class, method, descriptor)
            || compat_bind_refuses_unregistered(vm, class, method, descriptor)
            // zt t27/jd1 item 2 (`CRATONVM_JIT_COMPAT_BIND_ASKS_RESOLVER`).
            || compat_bind_yields_to_bytecode(vm, class, method, descriptor))
    {
        return None;
    }
    if !admission.admits_direct_bind(|| tail_would_run_the_native)
        || !policy.admits_shadow_bind(tail_would_run_the_native)
    {
        // t1 wave 3 (lane Y): into THIS VM's report sinks.
        cratonvm_jit::record_jdk_only_direct_native_refusal_into(
            vm.jit.jit_cache.jdk_only_sinks(),
            class,
            method,
            descriptor,
        );
        return None;
    }
    Some(shadow.entry_addr())
}

/// The compiled `invokedynamic` bridge — the one entry the codegen calls for
/// every indy site it does NOT lower to an uncommon trap.
///
/// The site metadata carries a `kind` tag
/// (`invokedynamic::jit_indy_site_kind`) saying which bootstrap family it is:
/// `StringConcatFactory`, or (since 2026-08-23) any of the bootstraps whose
/// implementation reaches the frame only through its operand stack and its
/// class id — `LambdaMetafactory`, `SwitchBootstraps`, `ObjectMethods`. The
/// second family is why a method that CREATES a lambda can stay compiled at
/// all; see `crate::runtime::invokedynamic::JitIndyGenericSite`.
///
/// Arguments reside in a JIT-owned raw spill buffer and are decoded by the call
/// site's descriptor before Java code can run or move the heap.
///
/// # Failure
///
/// The two kinds report differently, and deliberately. The concat bridge
/// returns `Option` and a `None` becomes `0`, which is what it has always done.
/// The generic bridge returns `Result`, and an `Err` is routed through
/// [`handle_jit_dispatch_error`] — the same stash-and-sentinel path a compiled
/// dispatch failure takes — because a bootstrap CAN throw
/// (`BootstrapMethodError`, `LambdaConversionError`) and swallowing that into a
/// null reference would surface it as an NPE several frames later.
///
/// SAFETY: all pointers are supplied by the generated call sequence for the
/// current live VM and the site allocation is process-lived.
pub unsafe extern "C" fn jit_indy_bridge(
    vm_ptr: i64,
    site_ptr: i64,
    args_ptr: *const i64,
    arg_count: i64,
) -> i64 {
    let f = || jit_indy_bridge_body(vm_ptr, site_ptr, args_ptr, arg_count);
    let bits = contain("jit_indy_bridge", OnPanic::Throw { vm_ptr }, i64::MIN, f);
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_indy_bridge`], run under its panic guard.
pub unsafe fn jit_indy_bridge_body(
    vm_ptr: i64,
    site_ptr: i64,
    args_ptr: *const i64,
    arg_count: i64,
) -> i64 {
    // JIT round 21 wave 1 (lane indy, INDY21-1, `CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC`):
    // a bimorphic guard's second arm passes the second target's guard cell as
    // its site. Its leaf is dispatched as the direct path below dispatches a
    // leaf; a cell whose terms fail hands back the instruction's own site, so
    // nothing below ever sees a cell. One load of the kind word otherwise.
    let (guarded_leaf, site_ptr) = crate::runtime::invokedynamic::jit_indy_guarded_leaf(
        vm_ptr as usize,
        site_ptr as usize,
        args_ptr,
        arg_count,
    );
    if let Some(leaf) = guarded_leaf {
        return jit_indy_leaf_dispatch(vm_ptr, leaf, args_ptr as i64, arg_count);
    }
    // Cast: a site address back to the helper ABI's word.
    let site_ptr = site_ptr as i64;
    // JIT round 16 wave 9 (lane indy, IN16-7; `CRATONVM_JIT_INDY_LINKED_DIRECT`,
    // default ON, `=0` off): a linked generic site whose `CallSite` target is a direct
    // handle of exactly the site's type calls that method as an ordinary
    // compiled invoke would, with the same argument words (the bridge's block
    // is the dispatch ABI's, and so are its return and exception protocols).
    // Before the JIT thread is derived: the dispatch derives its own.
    let kind = crate::runtime::invokedynamic::jit_indy_site_kind(site_ptr as usize);
    if kind == crate::runtime::invokedynamic::JIT_INDY_SITE_GENERIC && vm_ptr != 0 && arg_count >= 0 {
        if let Some(leaf) = crate::runtime::invokedynamic::jit_indy_linked_direct_leaf(
            &*(vm_ptr as *const SharedVm),
            site_ptr as usize,
            args_ptr,
            arg_count as usize,
        ) {
            return jit_indy_leaf_dispatch(vm_ptr, leaf, args_ptr as i64, arg_count);
        }
    }
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // `CRATONVM_DBG_JITC=1` — what the bridge was handed and what it answered.
    // A wrong result from a bridged site poses exactly one question, which SIDE
    // lost the value, and nothing else can tell the two apart: a helper that
    // returns a good pointer into a call sequence that drops it looks identical
    // to a helper that returned 0. Rides on the same variable the codegen's own
    // `indy bridge pc=` line uses, so one run shows the emission and the
    // execution together.
    let dbg = crate::runtime::env_cache::dbg_jitc();
    let count = arg_count.max(0) as usize;
    if dbg {
        eprintln!("[indy-bridge] kind={kind} args={count} args_ptr={args_ptr:?}");
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        // UNCONDITIONAL. This is a should-never-happen that answers with a
        // NULL REFERENCE, which is the worst shape a helper can have: no
        // exception, no crash, a wrong value that surfaces frames later. It is
        // inherited from the concat bridge, where it has always been silent.
        eprintln!(
            "[indy-bridge] NO JIT THREAD - returning a null result for a bridged invokedynamic (kind={kind}, args={count}). This is a bug, not a condition; the site's answer is now wrong."
        );
        return 0;
    };
    let vm = &*(vm_ptr as *const SharedVm);
    match kind {
        crate::runtime::invokedynamic::JIT_INDY_SITE_GENERIC => {
            match crate::runtime::invokedynamic::execute_jit_indy_generic_raw(
                vm,
                thread,
                site_ptr as usize,
                args_ptr,
                count,
            ) {
                Ok((bits, obj_opt)) => {
                    if dbg {
                        eprintln!("[indy-bridge] generic -> 0x{bits:x}");
                    }
                    if let Some(obj) = obj_opt {
                        // Object-return handoff root, same contract as every
                        // other JIT helper that hands a fresh reference back to
                        // compiled code (see `jit_integer_value_of_direct`):
                        // the value was allocated inside this call and nothing
                        // else roots it between here and the caller's store.
                        thread.native_pending_return = Some(obj);
                    }
                    bits
                }
                Err(error) => {
                    // `JitInvokeInfo` is only what `handle_jit_dispatch_error`
                    // uses for its diagnostics; the stash and the sentinel do
                    // not depend on it. A synthetic one keeps that message
                    // readable.
                    let info = JitInvokeInfo {
                        class_name: "<jit-indy>",
                        method_name: "bridge",
                        descriptor: "()Ljava/lang/Object;",
                        num_jit_args: 0,
                        return_type: b'L',
                        invoke_kind: 3,
                        declaring_class_id: 0,
                        owner_class_id: 0,
                    };
                    handle_jit_dispatch_error(vm, thread, error, &info)
                }
            }
        }
        _ => {
            match crate::runtime::invokedynamic::execute_jit_string_concat_raw(
                vm,
                thread,
                site_ptr as usize,
                args_ptr,
                count,
            ) {
                Ok(obj_opt) => {
                    let r = obj_opt.map(|obj| obj.as_ptr() as i64).unwrap_or(0);
                    if let Some(obj) = obj_opt {
                        // Same object-return handoff root as the generic arm:
                        // the String was allocated inside this call.
                        thread.native_pending_return = Some(obj);
                    }
                    if dbg {
                        eprintln!("[indy-bridge] concat -> 0x{r:x}");
                    }
                    r
                }
                // A throwing operand `toString()` (or the result's OOM). It
                // used to be flattened into a null `String`; route it through
                // the same stash-and-sentinel path as the generic arm, which
                // the call site's post-invoke exception check already reads.
                Err(error) => {
                    let info = JitInvokeInfo {
                        class_name: "<jit-indy>",
                        method_name: "concat",
                        descriptor: "()Ljava/lang/String;",
                        num_jit_args: 0,
                        return_type: b'L',
                        invoke_kind: 3,
                        declaring_class_id: 0,
                        owner_class_id: 0,
                    };
                    handle_jit_dispatch_error(vm, thread, error, &info)
                }
            }
        }
    }
}

/// The bridge's call of a bound direct leaf ([`jit_indy_bridge_body`]: the
/// linked-direct memo's leaf or a bimorphic guard's second arm), with the
/// bridge's own argument words.
///
/// JIT round 22 wave 1, lane runtime (INDY21-4;
/// `CRATONVM_JIT_INDY_LEAF_DIRECT_CALL`, opt-in, read once per VM): through
/// [`mh_bind_direct_call`], the constant-bound `MethodHandle` sites' lean door,
/// which enters the leaf's `DISPATCH_CACHE` / `VIRTUAL_DISPATCH_CACHE` entry
/// after asking every question the ladder asks before that hit (boundary note,
/// SATB flush, forwarded words, the native memos, class-identity flush,
/// redefinition / interpreter-only quiescence, depth guard) and hands every
/// other call to [`jit_invoke_dispatch_body`]. What the ladder runs first and
/// that door does not is run here: the inline-cache quiescent point. A leaf of
/// `java/lang/invoke/MethodHandle` (whose redirects the ladder runs first) or
/// a constructor (the ladder's direct throwable `<init>`) keeps the ladder.
/// The leaf sources already exclude a null receiver of a non-static leaf
/// (`jit_indy_linked_direct_leaf`, `jit_indy_guarded_leaf`), the one ladder
/// arm before its memo that a word decides. Off: the ladder, as before.
///
/// SAFETY: as [`jit_indy_bridge_body`]: `vm_ptr` is a live `SharedVm`;
/// `args_ptr` holds `arg_count` words in the dispatch ABI for `leaf`.
pub unsafe fn jit_indy_leaf_dispatch(
    vm_ptr: i64,
    leaf: &'static JitInvokeInfo,
    args_ptr: i64,
    arg_count: i64,
) -> i64 {
    let leaf_ptr = leaf as *const JitInvokeInfo as i64;
    if vm_ptr == 0
        || leaf.class_name == "java/lang/invoke/MethodHandle"
        || leaf.method_name == "<init>"
        || !crate::runtime::interpreter::jit_bridge::indy_leaf_direct_call_enabled(
            &*(vm_ptr as *const SharedVm),
        )
    {
        return jit_invoke_dispatch_body(vm_ptr, leaf_ptr, args_ptr, arg_count);
    }
    // The ladder's first step (JIT round 14 wave 2, lane mic, M8-1): this
    // thread has no inline-cache probe in flight.
    cratonvm_jit::jit_ic_quiescent_point();
    mh_bind_direct_call(vm_ptr, leaf, args_ptr, arg_count)
}

/// Fallible cold wrapper allocation for compiled boxing on every collector.
/// A refusal collects and retries, then declines to the native funnel's OOME
/// channel. No unwind may escape into the compiled caller.
///
/// zs s6/no: the forced collection runs even when the GC-overhead verdict
/// already stands (see the latched arm below and
/// `CRATONVM_ZGC_JIT_BOX_COLLECTS_LATCHED`, default ON, `=0` restores the
/// skip).
pub fn jit_box_cold_alloc(
    vm: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        if let Some(object) = ctx.try_alloc_object(class_id, 1) {
            return Some(object);
        }
    }
    // zs s6/no: a latched GC-overhead streak no longer skips the collection
    // (`CRATONVM_ZGC_JIT_BOX_COLLECTS_LATCHED`, default ON, `=0` restores the
    // skip). The skip made this the one ZGC refusal door that throws with NO
    // collection while the streak stands: a compiled `Integer.valueOf` after
    // the program dropped its data (`catch (OutOfMemoryError e) { cache =
    // null; }`) fell through to a native allocation that never collects, with
    // the heap full of garbage (zr z7/rv6 report, finding 10). The
    // interpreter's ladder (`collect_and_retry_with_thread`) runs its forced
    // collection first whatever the streak says, then judges; HotSpot's ZGC
    // has no overhead limit and stalls into a collection. The futile rule
    // still saves the collection of an unchanged heap (and the owed one is
    // paid below), so a heap wedged on live data pays one collection per
    // refusal here, as at every other door.
    let latched = crate::runtime::interpreter::gc_overhead_limit_exceeded(vm);
    if !latched || true {
        // zt t1/tp (X2): counted where the collection runs, so the `=0`
        // latched arm's skipped collections are not counted.
        vm.mem
            .alloc_ladder
            .jit_box_collects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        thread.tlab.retire();
        crate::runtime::interpreter::maybe_gc_forced_pub_at(vm, thread, "jit-helpers");
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        if let Some(object) = ctx.try_alloc_object(class_id, 1) {
            return Some(object);
        }
        // zr z7/rv6: the futile-refusal rule (zr z5/cr) may have skipped the
        // forced collection above ("jit-helpers" is a skippable site) on the
        // promise that the ladder's clear-soft rung pays it. This door has no
        // such rung: its fall-through raises the OutOfMemoryError from a
        // native door that never collects. So pay the owed collection here
        // and retry once -- a window under 4 KiB says nothing about garbage
        // the program DROPPED since the last collection. No-op unless the rule
        // fired (`CRATONVM_ZGC_REFUSAL_SKIP_FUTILE=0` restores the pre-cr door).
        if crate::runtime::interpreter::gc_and_alloc::zgc_refusal_pay_owed_now(vm, thread) {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            if let Some(object) = ctx.try_alloc_object(class_id, 1) {
                return Some(object);
            }
        }
    }
    vm.mem
        .alloc_ladder
        .jit_box_funnel_fallthroughs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    None
}


/// Thin direct-call target for JIT `invokestatic Integer.valueOf(I)` sites
/// (registered into `cratonvm_jit::INTEGER_VALUE_OF_DIRECT_FN` by
/// `build_helpers`; the recognition lives in `jit::try_compile`).
///
/// Semantics are identical to the `IntegerNativeKind::ValueOf` arm of
/// [`call_integer_native_raw`], minus the generic `jit_invoke_dispatch`
/// round trip (info decode, per-call thread-local cache probes, argument
/// buffer build):
///  * out-of-range values allocate a fresh wrapper through the mutator's
///    normal native-context TLAB path and publish it via
///    `native_pending_return` (the established JIT→native object-return
///    handoff root);
///  * `-128..=127` (and the cold pre-discovery case, and any class-redefine
///    window) route through `safe_native_call` to the canonical native
///    callback, preserving the JLS identity-cache contract;
///  * errors (OOM) route through `handle_jit_dispatch_error` exactly like
///    the dispatch helper, so the returned sentinel carries properly
///    stashed exception state for the caller's post-invoke check.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_integer_value_of_direct(
    vm_ptr: i64,
    value: i64,
) -> i64 {
    let f = || jit_integer_value_of_direct_body(vm_ptr, value);
    let bits = contain(
        "jit_integer_value_of_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_integer_value_of_direct`], run under its panic guard.
pub unsafe fn jit_integer_value_of_direct_body(vm_ptr: i64, value: i64) -> i64 {
    jit_integer_value_of_direct_body_on(vm_ptr, value, None)
}

/// The box helpers' entry bookkeeping. From compiled code (`held == false`):
/// [`jit_safepoint_flush_satb`], whose `native_pending_return` drain takes the
/// JIT thread. From a caller that already HOLDS that thread (`_on` with
/// `Some`, the lambda arm; r23 w3 review): the SATB flush alone -- the drain's
/// `jit_thread_mut` would be the sibling borrow the `_on` split exists to
/// avoid (a debug-build `JIT_THREAD_BORROWED` trip, aliasing `&mut` in
/// release), and the funnel this replaces never drained there either.
pub unsafe fn jit_box_helper_entry_flush(vm_ptr: i64, held: bool) {
    if !held {
        jit_safepoint_flush_satb(vm_ptr);
    } else if vm_ptr != 0 {
        // SAFETY: as `jit_safepoint_flush_satb`: a live SharedVm pointer.
        let vm = &*(vm_ptr as *const SharedVm);
        vm.mem.heap.flush_thread_satb();
    }
}

