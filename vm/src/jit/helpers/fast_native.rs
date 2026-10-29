// `(vm_ptr, varhandle, receiver)` `extern "C"` function per
// (access mode, primitive return kind) slot.
//
// One implementation, reached two ways. `try_varhandle_instance_field_read`
// below serves the SAME read from inside `jit_invoke_dispatch`, for every site
// this bind declines and for the interpreter's dispatches; both call
// `varhandle_instance_field_read_bits`, so the two routes cannot drift apart
// in what they consider a servable handle.
// ---------------------------------------------------------------------------

/// Erased call-site descriptors for [`VARHANDLE_READ_INFOS`], indexed by the
/// return-kind half of a slot.
///
/// A baked direct call has no `JitInvokeInfo` of its own (see
/// `bytecode_walk.rs`: `info_ptr` is `None` for a thin helper), so the cold arm
/// cannot hand the generic dispatcher the site's real descriptor. These stand
/// in for it.
///
/// For a PRIMITIVE return the substitution is not observable: the descriptor's
/// only readers are the argument decode (one reference coordinate either way),
/// `unbox_poly_return_checked`'s return-type rules and `coerce_native_return`,
/// and all three see the identical return char.
///
/// For a REFERENCE return there is one reader that CAN tell the difference —
/// `varhandle_reference_return_mismatch`, which compares against the site's
/// declared class — and the two reference kinds exist to answer it without a
/// descriptor. `REF_OBJECT` needs no answer (its stand-in IS its real
/// descriptor, and no box is refused at an `Object` return); `REF_STRICT` names
/// a site no box can satisfy at all, so `varhandle_read_direct_reference`
/// raises on any box the cold arm produces. See
/// `cratonvm_jit::VARHANDLE_BOX_ACCEPTING_RETURNS` for the thirteen types that
/// fall between the two and are therefore not bound.
const VARHANDLE_READ_DESCRIPTORS: [&str; cratonvm_jit::VARHANDLE_READ_KINDS] = [
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
static VARHANDLE_READ_INFOS: [JitInvokeInfo; cratonvm_jit::VARHANDLE_READ_SLOTS] = [
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
const fn vh_read_info(mode: usize, kind: usize) -> JitInvokeInfo {
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
struct VarHandlePrimitiveReadProof {
    vm: usize,
    vh: usize,
    receiver: usize,
    heap_epoch: u64,
    metadata_generation: u64,
    layout_generation: u64,
    vh_class: u32,
    vh_compact: bool,
    receiver_class: u32,
    site_return: u8,
    judged: bool,
    exact_slot: Option<usize>,
    exact_offset: Option<usize>,
    value_offset: usize,
    storage: cratonvm_types::FieldStorageKind,
}

const VH_PRIMITIVE_READ_PROOF_SLOTS: usize = 64;
thread_local! {
    static VH_PRIMITIVE_READ_PROOFS:
        [std::cell::Cell<Option<VarHandlePrimitiveReadProof>>; VH_PRIMITIVE_READ_PROOF_SLOTS] =
        const { [const { std::cell::Cell::new(None) }; VH_PRIMITIVE_READ_PROOF_SLOTS] };
    static VH_PRIMITIVE_READ_PROOF_REPORTED_VM: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[inline]
fn vh_primitive_read_proof_slot(vh: usize, receiver: usize, ret: u8) -> usize {
    ((vh >> 4).wrapping_mul(31) ^ (receiver >> 4) ^ usize::from(ret))
        & (VH_PRIMITIVE_READ_PROOF_SLOTS - 1)
}

/// The hit cannot allocate or safepoint. Forwarding remains a live read on
/// both objects; their storage bit and class must still agree. The exact
/// flag is read on every judged hit, so even a changed final field cannot
/// turn an exact handle into an unchecked access.
// SAFETY: Performs raw field reads within verified bounds without safepoint or allocation.
unsafe fn varhandle_cached_primitive_read(
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
unsafe fn remember_varhandle_primitive_read(
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
        None => None,
    };
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
unsafe fn varhandle_read_direct_impl(
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
unsafe fn varhandle_read_direct_reference(
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
unsafe fn varhandle_strict_reference_return_check(vm: &SharedVm, slot: usize, bits: i64) -> i64 {
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
    let retired = vm.config.vh_w6_1_retired;
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
unsafe extern "C" fn jit_varhandle_read_direct_uncached<const SLOT: usize>(
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
fn varhandle_read_direct_fns() -> [usize; cratonvm_jit::VARHANDLE_READ_SLOTS] {
    // Selected under the VM's flag snapshot at compilation, never polled on
    // the hot read path. No registration, native kind or ABI changes.
    let cached =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_PRIMITIVE_READ_CACHE");
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
unsafe fn varhandle_write_value_of(vm: &SharedVm, kind: u8, bits: i64) -> Option<Value> {
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
unsafe fn varhandle_write_direct_impl(
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
/// `vm_exec::publish_overwritten_field` (now `pub(crate)`), with the store's
/// descriptor and the native store's kill switch (`CRATONVM_NATIVE_STORE_SATB`),
/// so the decision (marking Generational heap, a slot that may hold a
/// reference, the switch asked last) cannot drift from the native's.
#[inline]
fn thin_store_publish_overwritten_field(
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

#[cfg(test)]
mod nio21_w4_thin_store_satb_tests {
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_gc::{ConcurrentGcState, SatbQueue};
    use std::sync::Arc;

    // nio21 wave 5 L1: the pure decision test of the wave-4 copy
    // (`thin_store_must_publish`) went with the copy; the decision is
    // `vm_exec::publish_overwritten_field`'s, tested by
    // `r16w9_native_store_satb_tests` against a mock heap.

    /// The bug the page records: a thin `VarHandle` reference write on a
    /// marking Generational heap must log the value it overwrites, as the
    /// native's `NativeContextImpl::set_field*` does.
    #[test]
    fn nio21_w4_thin_store_publishes_the_overwritten_reference_while_generational_marks() {
        // The overwritten value is YOUNG: pin the old-only SATB filter off so
        // the barrier alone decides (as `jit_putfield_object_satb_pre_barrier_enqueues_old_ref`).
        let _flags = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_GEN_SATB_OLD_ONLY",
                Some("0"),
            )]),
        );
        let cfg = VmConfig {
            gc_algorithm: crate::config::GcAlgorithm::Generational,
            ..VmConfig::default()
        };
        let mut vm_box: Box<SharedVm> = Box::new(SharedVm::new(cfg));
        let satb: Arc<SatbQueue> = Arc::new(SatbQueue::new());
        let state: Arc<ConcurrentGcState> = Arc::new(ConcurrentGcState::new());
        vm_box
            .mem
            .heap
            .enable_concurrent_gc(satb.clone(), state.clone());
        let heap = &vm_box.mem.heap;
        let container = heap.alloc_object(ClassId::new(0), 1);
        let old_obj = heap.alloc_object(ClassId::new(0), 0);
        heap.set_field(container, 0, Value::Object(Some(old_obj)));

        // Nothing marks: nothing is read or published.
        assert!(!thin_store_publish_overwritten_field(
            heap, container, 0, b'L'
        ));

        satb.activate();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
        heap.flush_thread_satb();
        let _ = satb.drain();

        // A primitive descriptor never publishes; a reference one does.
        assert!(!thin_store_publish_overwritten_field(
            heap, container, 0, b'I'
        ));
        assert!(thin_store_publish_overwritten_field(
            heap, container, 0, b'L'
        ));
        heap.flush_thread_satb();
        let drained = satb.drain();
        assert!(
            drained.contains(&(old_obj.as_ptr() as usize)),
            "the thin store's pre-barrier must log the OLD reference; drained={drained:?}"
        );

        // A null old value publishes nothing.
        heap.set_field(container, 0, Value::Object(None));
        assert!(!thin_store_publish_overwritten_field(
            heap, container, 0, b'L'
        ));

        satb.deactivate_and_discard();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
    }
}

/// The store itself, shared by the direct arm and the funnel-side twin so the
/// two cannot drift apart in what they consider a servable handle.
///
/// `true` when the write happened; `false` declines to the caller's cold arm.
///
/// # SAFETY
///
/// `vh_raw` and `recv_raw` are raw references a compiled frame is holding.
unsafe fn varhandle_instance_field_write(
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
fn varhandle_write_direct_fns() -> [usize; cratonvm_jit::VARHANDLE_WRITE_SLOTS] {
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
const VARHANDLE_WRITE_DESCRIPTORS: [&str; 10] = [
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
static VARHANDLE_WRITE_INFOS: [JitInvokeInfo; cratonvm_jit::VARHANDLE_WRITE_SLOTS] = [
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
const fn vh_write_info(mode: usize, kind: usize) -> JitInvokeInfo {
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
unsafe fn varhandle_instance_field_cas_shared(
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
unsafe fn varhandle_instance_field_cas_operands(
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
    // (`tools/probes/Nio21VarHandleConst.java` row `exc wrongExpected`). This
    // arm cannot raise it; it declines to the registered native, which owns
    // that answer (`r21-varhandle-instance-cas-does-not-cast-expected-FIXED-20261009.md`).
    // `CRATONVM_JIT_VH_CAS_EXPECTED_SCREEN=0` restores the old acceptance.
    // The switch is read only on a refusal, off the common path.
    if !varhandle_plan_admits_value(vm, &plan, &expected)
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_CAS_EXPECTED_SCREEN")
    {
        return None;
    }
    let receiver = heap.load_and_forward(receiver);
    Some((receiver, plan.field_index as usize, expected, new_val)) // Cast: field slot index
}

/// The `compareAndSet` operand kind of an interpreter call site (`descriptor`,
/// whose return the caller has checked is `Z`): the common kind of `expected`
/// and `new` when the site is exactly `(receiver, expected, new)` with both
/// of one kind ([`varhandle_cas_operand_kinds`], references collapsed to `L`),
/// else `None`. Read once per site by the interpreter's signature-polymorphic
/// site cache (`sig_poly_site`, JIT round 16 wave 7 lane varhandle, MH16-6).
pub(crate) fn varhandle_cas_site_kind(descriptor: &str) -> Option<u8> {
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
pub(crate) fn varhandle_instance_field_cas_values(
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
fn vh_interp_operand_word(value: Value, site_kind: u8) -> Option<i64> {
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
pub(crate) fn varhandle_cae_site_kind(descriptor: &str, ret_tag: u8) -> Option<u8> {
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
pub(crate) fn varhandle_instance_field_cae_values(
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
fn compare_and_exchange_field_hw(
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
unsafe fn varhandle_cas_direct_impl(
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
fn varhandle_cas_direct_fns() -> [usize; cratonvm_jit::VARHANDLE_CAS_SLOTS] {
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
const VARHANDLE_CAS_DESCRIPTORS: [&str; 10] = [
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
unsafe fn varhandle_thin_call_site(word: i64) -> (i64, *const JitInvokeInfo) {
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
unsafe fn varhandle_thin_fast_arm_admitted(
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
fn varhandle_thin_cold_arm_info(
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

#[cfg(test)]
mod r15w6_vh8_judged_thin_call {
    use super::*;
    use crate::config::VmConfig;

    /// The first word decodes to the context and no site when untagged (every
    /// unjudged bind), and to the record's `SharedVm` and its own info when
    /// tagged; the cold arm then dispatches with that info, not the stand-in.
    #[test]
    fn the_tagged_word_is_the_site_and_the_plain_word_is_the_vm() {
        let record = cratonvm_jit::VarHandleJudgedSite::new(
            0x7f00_1000,
            0x7f00_2000,
            "java/lang/invoke/VarHandle",
            "get",
            "(LHolder;)I",
            2,
            b'I',
            0,
            0,
            0,
        );
        let addr = &*record as *const cratonvm_jit::VarHandleJudgedSite as usize;
        // SAFETY: an untagged word is returned as it is, never read.
        let (vm, site) = unsafe { varhandle_thin_call_site(0x7f00_1000) };
        assert_eq!((vm, site.is_null()), (0x7f00_1000, true));
        let tagged = (addr | cratonvm_jit::VARHANDLE_JUDGED_SITE_TAG) as i64;
        // SAFETY: `tagged` is `record`'s address with the tag, alive here.
        let (vm, site) = unsafe { varhandle_thin_call_site(tagged) };
        assert_eq!(vm, 0x7f00_1000);
        assert_eq!(site, &record.info as *const JitInvokeInfo);
        let stand_in = &VARHANDLE_READ_INFOS[4];
        assert_eq!(varhandle_thin_cold_arm_info(site, stand_in), site as i64);
        assert_eq!(
            varhandle_thin_cold_arm_info(std::ptr::null(), stand_in),
            stand_in as *const JitInvokeInfo as i64
        );
        assert!(
            !is_varhandle_stand_in_info(&record.info),
            "the real site is judged"
        );
    }

    /// The fast arm stays open for an unjudged bind, before any exact mint,
    /// and for a null handle (which it declines itself).
    #[test]
    fn the_fast_arm_closes_only_for_a_judged_exact_receiver() {
        let shared = SharedVm::new(VmConfig::default());
        let info = &VARHANDLE_WRITE_INFOS[0];
        // SAFETY: no handle is read: the gate is closed, or the word is null.
        unsafe {
            assert!(varhandle_thin_fast_arm_admitted(
                &shared,
                0x1000,
                std::ptr::null()
            ));
            assert!(varhandle_thin_fast_arm_admitted(&shared, 0x1000, info));
            shared
                .natives
                .var_handle_exact_slot
                .store(1, std::sync::atomic::Ordering::Relaxed);
            assert!(varhandle_thin_fast_arm_admitted(&shared, 0, info));
            assert!(varhandle_thin_fast_arm_admitted(
                &shared,
                0x1000,
                std::ptr::null()
            ));
        }
    }
}

/// Is `info` one of the thin `VarHandle` helpers' synthetic cold-arm sites
/// ([`VARHANDLE_READ_INFOS`], [`VARHANDLE_WRITE_INFOS`],
/// [`VARHANDLE_CAS_INFOS`])? Their descriptor is a stand-in shared by every
/// bound site of the slot, not the call site's, so nothing that compares the
/// call site's exact type (an invoke-exact `VarHandle`, interpreter round i1
/// wave 37, lane L4) may read it.
fn is_varhandle_stand_in_info(info: &JitInvokeInfo) -> bool {
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
/// `--jdk-only` and `CRATONVM_JIT_VARHANDLE_*_DIRECT_HELPERS=0` already take.
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
pub(crate) fn varhandle_thin_binds_admitted(shared: &SharedVm) -> bool {
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
pub(crate) fn varhandle_method_entry_binds_admitted(shared: &SharedVm) -> bool {
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
pub(crate) fn varhandle_read_slot_bindable(slot: usize) -> bool {
    cratonvm_jit::varhandle_read_slot_kind(slot) != cratonvm_jit::VARHANDLE_READ_KIND_REF_STRICT
        || !cratonvm_native_builtins::lang_invoke::vh_reference_return_checkcast_enabled()
}

/// `CRATONVM_JIT_SPLICE_VH_IR_BOUND` (JIT round 20 wave 11, lane pIndy,
/// W14L4-2 = W12L2-3 of `jit-r21-nio-proposals-w14-L4.md`; **default ON**,
/// `=0` restores the refusal): see [`varhandle_splice_keeps_thin_bind`]. Read
/// per splice-resolution question (compile time only), no static.
fn splice_vh_ir_bound_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SPLICE_VH_IR_BOUND")
}

/// Should the VM's splice planner (`jit_bridge.rs` `resolve_inline_site_from`,
/// `thin-bound-native-keeps-its-own-fast-path`) refuse to splice a callee
/// because its body calls `java/lang/invoke/VarHandle.<name><desc>`? Only when
/// the callee's own MethodEntry compile will bind that site to a thin helper
/// (round 15 wave 4, lane vh5; VH3-5,
/// `r15w3-vh3-splice-refusal-ignores-unwired-varhandle-binds-FIXED-20260930.md`):
/// spliced, the site becomes an ordinary dispatch, which costs nothing when
/// the callee's site would dispatch generically anyway.
///
/// The questions are the single-pass bind ladder's (`jit/src/lib.rs`, the
/// `VarHandle` read / write / `compareAndSet` arms): the raw-callee gate, the
/// family's switch, the slot, this VM's exact-handle gate
/// ([`varhandle_thin_binds_admitted`]), a read slot's cast rule
/// ([`varhandle_read_slot_bindable`]), and the door's native policy
/// ([`direct_native_bind_admitted`] on the ERASED registered descriptor, which
/// is where `--jdk-only` admits or refuses a `Bridge` bind). Before, the
/// refusal asked only "is it a read or write slot": it kept one-line wrappers
/// unspliced after the first exact mint and for `REF_STRICT` reads (no bind),
/// and spliced `compareAndSet` wrappers whose CAS the callee would have bound.
///
/// `CRATONVM_JIT_SPLICE_VH_REFUSAL_ASKS_BIND=0`
/// (`lang_invoke::vh_splice_refusal_asks_bind_enabled`) restores that answer.
/// `CRATONVM_JIT_SPLICE_THIN_NIO_MD_ASKS_BIND` (netty compression wall,
/// 2026-10-14; default ON, `=0` restores the unconditional refusal): see
/// [`nio_byte_splice_keeps_thin_bind`]. Read per question (compile time).
fn splice_thin_nio_md_asks_bind_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SPLICE_THIN_NIO_MD_ASKS_BIND")
}

/// Would a spliced body's `ByteBuffer.get(int)` / `put(int, byte)` call
/// (`write`) have been bound to the thin `DirectByteBuffer` element helper by
/// the callee's own compile -- the reason the VM resolver refuses to splice a
/// body holding one (`thin-bound-native-keeps-its-own-fast-path`)?
///
/// The binds' own two questions, asked without a call-site profile (the call
/// is inside the callee): the registry admits the implementing triple
/// (`--jdk-only`), and the helper has served a `DirectByteBuffer` in that
/// direction (`elem_fastpath::class_is_served`, the table the bind gate
/// `nio_byte_element_bind_refused` and the helper's own prologue read). A
/// helper that has never served the class binds nowhere, so the splice loses
/// nothing. Compile time only.
pub(crate) fn nio_byte_splice_keeps_thin_bind(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    write: bool,
) -> bool {
    if !splice_thin_nio_md_asks_bind_enabled() {
        return true;
    }
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
/// switch and the `--jdk-only` admission the bind asks
/// (`md_update_direct_helper_enabled`, `direct_native_helper`). Compile time
/// only.
pub(crate) fn md_update_splice_keeps_thin_bind(shared: &SharedVm) -> bool {
    if !splice_thin_nio_md_asks_bind_enabled() {
        return true;
    }
    // `--compatible` asks the door's question too once the bind does
    // (SYNC22-3, JIT r23 w1 L6, `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY`).
    let asks = crate::vm::dispatch_policy(shared).is_jdk_only()
        || compat_bind_asks_registry_enabled(shared);
    cratonvm_jit::md_update_direct_helper_enabled()
        && (!asks
            || direct_native_bind_admitted(shared, "java/security/MessageDigest", "update", "(B)V"))
}

pub(crate) fn varhandle_splice_keeps_thin_bind(
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
    if ir_mode && splice_vh_ir_bound_enabled() {
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
        "([Ljava/lang/Object;)Z"
    } else {
        return false;
    };
    direct_native_bind_admitted(shared, "java/lang/invoke/VarHandle", name, erased)
}

#[cfg(test)]
mod r15w4_vh5_splice_keeps_thin_bind {
    use super::*;
    use crate::config::VmConfig;

    /// VH3-5: a `compareAndSet` site is protected like a read, and the first
    /// exact mint withdraws the protection with the binds.
    #[test]
    fn the_refusal_follows_the_bind() {
        if !cratonvm_native_builtins::lang_invoke::vh_splice_refusal_asks_bind_enabled()
            || !cratonvm_jit::direct_jit_callee_calls_enabled()
            || !cratonvm_jit::varhandle_cas_direct_helpers_enabled()
            || !cratonvm_jit::varhandle_read_direct_helpers_enabled()
        {
            return; // an explicit `0` in the test environment wins
        }
        let shared = SharedVm::new(VmConfig::default());
        let cas = "(Ljava/lang/Object;II)Z";
        let read = "(Ljava/lang/Object;)I";
        assert!(cratonvm_jit::varhandle_cas_helper_slot("compareAndSet", cas).is_some());
        assert!(varhandle_splice_keeps_thin_bind(
            &shared,
            "compareAndSet",
            cas,
            false
        ));
        assert!(varhandle_splice_keeps_thin_bind(
            &shared, "get", read, false
        ));
        // JIT round 20 wave 11 (lane pIndy, W14L4-2): an optimizing-tier
        // splice whose call the IR bind loop binds keeps the bind.
        let ir_bound = |shared: &SharedVm, name: &str, desc: &str| {
            splice_vh_ir_bound_enabled()
                && cratonvm_jit::varhandle_ir_bound_family(name, desc).is_some_and(|erased| {
                    varhandle_method_entry_binds_admitted(shared)
                        && direct_native_bind_admitted(
                            shared,
                            "java/lang/invoke/VarHandle",
                            name,
                            erased,
                        )
                })
        };
        assert_eq!(
            varhandle_splice_keeps_thin_bind(&shared, "get", read, true),
            !cratonvm_jit::varhandle_ir_read_shape("get", read) && !ir_bound(&shared, "get", read)
        );
        assert_eq!(
            varhandle_splice_keeps_thin_bind(&shared, "compareAndSet", cas, true),
            !ir_bound(&shared, "compareAndSet", cas)
        );
        // Not a thin-bindable shape (an array element read): never refused.
        assert!(!varhandle_splice_keeps_thin_bind(
            &shared, "get", "([II)I", false
        ));
        shared
            .natives
            .var_handle_exact_slot
            .store(1, std::sync::atomic::Ordering::Relaxed);
        if varhandle_thin_binds_admitted(&shared) {
            return; // `CRATONVM_JIT_VH_THIN_BIND_EXACT_GATE=0`
        }
        // Round 15 wave 6 (lane vh8): a judged ladder keeps binding after the
        // mint, so the protection stays with it.
        let judged = cratonvm_jit::varhandle_judged_binds_enabled();
        assert_eq!(
            varhandle_splice_keeps_thin_bind(&shared, "compareAndSet", cas, false),
            judged
        );
        assert_eq!(
            varhandle_splice_keeps_thin_bind(&shared, "get", read, false),
            judged
        );
        assert_eq!(
            varhandle_splice_keeps_thin_bind(&shared, "get", read, true),
            judged
                && !cratonvm_jit::varhandle_ir_read_shape("get", read)
                && !ir_bound(&shared, "get", read)
        );
    }
}

#[cfg(test)]
mod r15w2_vh2_thin_bind_exact_gate {
    use super::*;
    use crate::config::VmConfig;

    /// The first exact mint unwires the three `VarHandle` tables of every
    /// later compile's `direct_helper_table_for`, and nothing else.
    #[test]
    fn an_exact_mint_unwires_every_varhandle_thin_bind() {
        let shared = SharedVm::new(VmConfig::default());
        let open = direct_helper_table_for(&shared);
        // Every read slot VH2-3 leaves bindable is wired.
        assert!(open
            .varhandle_read
            .iter()
            .enumerate()
            .all(|(slot, &a)| (a != 0) == varhandle_read_slot_bindable(slot)));
        assert!(open
            .varhandle_write
            .iter()
            .chain(&open.varhandle_cas)
            .all(|&a| a != 0));
        shared
            .natives
            .var_handle_exact_slot
            .store(1, std::sync::atomic::Ordering::Relaxed);
        let closed = direct_helper_table_for(&shared);
        if cratonvm_jit::varhandle_judged_binds_enabled() {
            // Round 15 wave 6 (lane vh8): the ladder binds judged rows, which
            // need no withdrawal, so the table stays wired across the mint.
            assert_eq!(closed.varhandle_read, open.varhandle_read);
            assert_eq!(closed.varhandle_write, open.varhandle_write);
            assert_eq!(closed.varhandle_cas, open.varhandle_cas);
        } else {
            assert!(closed
                .varhandle_read
                .iter()
                .chain(&closed.varhandle_write)
                .chain(&closed.varhandle_cas)
                .all(|&a| a == 0));
        }
        assert_eq!(closed.integer_value_of, open.integer_value_of);
        assert_eq!(closed.static_base_resolver, open.static_base_resolver);
    }

    /// VH2-3: exactly the `REF_STRICT` read slots (a site declaring a class
    /// other than `Object`) are left unbound while reference returns are cast.
    #[test]
    fn only_ref_strict_read_slots_are_withheld_for_the_cast() {
        if !cratonvm_native_builtins::lang_invoke::vh_reference_return_checkcast_enabled() {
            return; // an explicit `0` in the test environment wins
        }
        for slot in 0..cratonvm_jit::VARHANDLE_READ_SLOTS {
            assert_eq!(
                varhandle_read_slot_bindable(slot),
                cratonvm_jit::varhandle_read_slot_kind(slot)
                    != cratonvm_jit::VARHANDLE_READ_KIND_REF_STRICT,
                "slot {slot}"
            );
        }
        // `None` only under the reference half's own kill switch.
        let strict = "(Ljava/lang/Object;)Ljava/lang/String;";
        if let Some(slot) = cratonvm_jit::varhandle_read_helper_slot("get", strict) {
            assert!(!varhandle_read_slot_bindable(slot));
        }
        let object = "(Ljava/lang/Object;)Ljava/lang/Object;";
        if let Some(slot) = cratonvm_jit::varhandle_read_helper_slot("get", object) {
            assert!(varhandle_read_slot_bindable(slot));
        }
    }
}

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
unsafe fn jit_var_handle_exact_refusal(
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
unsafe fn jit_exact_var_handle_receiver(
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
unsafe fn note_thin_varhandle_helper_unjudged(vm: &SharedVm, vh: i64, family: &str, slot: usize) {
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
/// signature-polymorphic block serves the access unjudged. Under `--jdk-only`
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
unsafe fn jit_mic_var_handle_exact_refusal(
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
pub(crate) fn retire_varhandle_thin_bound_bodies(shared: &SharedVm) -> usize {
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
pub(crate) fn varhandle_thin_bound_artifact_is_stale(
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
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_VH_THIN_BIND_PUBLISH_REFUSAL",
        )
}

#[cfg(test)]
mod r15w3_vh3_thin_bind_retire {
    use super::*;
    use crate::config::VmConfig;

    /// A body that recorded the thin-bind dependency is withdrawn by the
    /// first-mint sweep; a body that did not is kept, and the dependency is
    /// recorded once however many sites bind.
    #[test]
    fn the_first_mint_sweep_withdraws_only_thin_bound_bodies() {
        if !cratonvm_jit::varhandle_thin_bind_retire_enabled() {
            return; // an explicit `0` in the test environment wins
        }
        let shared = SharedVm::new(VmConfig::default());
        let cache = &shared.jit.jit_cache;
        let cid = ClassId::new(0);
        let Some(bound_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
            return;
        };
        let Some(plain_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
            return;
        };
        let mut bound = cratonvm_jit::CompiledMethod::new(bound_buf);
        cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
        cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
        assert_eq!(bound.inlined_methods.len(), 1, "recorded once");
        cache.put("T".into(), "bound".into(), "()V".into(), cid, bound);
        let plain = cratonvm_jit::CompiledMethod::new(plain_buf);
        cache.put("T".into(), "plain".into(), "()V".into(), cid, plain);
        assert_eq!(retire_varhandle_thin_bound_bodies(&shared), 1);
        assert!(cache.get("T", "bound", "()V", cid).is_none());
        assert!(cache.get("T", "plain", "()V", cid).is_some());
        // Idempotent: nothing left to withdraw.
        assert_eq!(retire_varhandle_thin_bound_bodies(&shared), 0);
    }

    /// Loading a `VarHandle` subclass (the class-define listener invalidates
    /// by every supertype) does not withdraw a thin-bound body: the dependency
    /// names no real class.
    #[test]
    fn a_varhandle_subclass_define_keeps_thin_bound_bodies() {
        if !cratonvm_jit::varhandle_thin_bind_retire_enabled() {
            return;
        }
        let shared = SharedVm::new(VmConfig::default());
        let cache = &shared.jit.jit_cache;
        let cid = ClassId::new(0);
        let Some(buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
            return;
        };
        let mut bound = cratonvm_jit::CompiledMethod::new(buf);
        cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
        cache.put("T".into(), "bound".into(), "()V".into(), cid, bound);
        assert_eq!(
            cache.invalidate_for_class_change("java/lang/invoke/VarHandle"),
            0
        );
        assert!(cache.get("T", "bound", "()V", cid).is_some());
    }
}

#[cfg(test)]
mod r15w3_vh4_thin_bound_publication {
    use super::*;
    use crate::config::VmConfig;

    /// VH3-1: a thin-bound artifact is stale at publication exactly when the
    /// VM no longer admits thin binds; an artifact without the dependency
    /// never is.
    #[test]
    fn a_thin_bound_artifact_is_refused_once_binds_are_not_admitted() {
        if !cratonvm_jit::varhandle_thin_bind_retire_enabled()
            || !cratonvm_native_builtins::lang_invoke::vh_thin_bind_exact_gate_enabled()
            || !cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_VH_THIN_BIND_PUBLISH_REFUSAL",
            )
        {
            return; // an explicit `0` in the test environment wins
        }
        let shared = SharedVm::new(VmConfig::default());
        let Some(bound_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
            return;
        };
        let Some(plain_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
            return;
        };
        let mut bound = cratonvm_jit::CompiledMethod::new(bound_buf);
        cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
        let plain = cratonvm_jit::CompiledMethod::new(plain_buf);
        // Gate closed: binds are admitted, nothing is stale.
        assert!(!varhandle_thin_bound_artifact_is_stale(&shared, &bound));
        assert!(!varhandle_thin_bound_artifact_is_stale(&shared, &plain));
        shared
            .natives
            .var_handle_exact_slot
            .store(1, std::sync::atomic::Ordering::Relaxed);
        assert!(varhandle_thin_bound_artifact_is_stale(&shared, &bound));
        assert!(!varhandle_thin_bound_artifact_is_stale(&shared, &plain));
    }
}

/// Judge the exact `VarHandle` `vh` at the compiled site `info`: with
/// `info`'s own descriptor, or, for a thin helper's stand-in site, with the
/// descriptor its compiled caller's invoke names
/// ([`varhandle_stand_in_site_descriptor`]). A stand-in whose real site
/// cannot be recovered is not judged (the pre-wave-49 answer), which
/// `CRATONVM_DBG_MH_STACK=1` reports.
#[cold]
#[inline(never)]
fn jit_var_handle_exact_judgement(
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
const VH_EXACT_SITE_MEMO_CAP: usize = 8;

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
fn varhandle_stand_in_site_descriptor_memoized(
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
fn jit_vh_astype_refusal_site_text(
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
fn varhandle_stand_in_site_descriptor(vm: &SharedVm, info: &JitInvokeInfo) -> Option<String> {
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
static VARHANDLE_CAS_INFOS: [JitInvokeInfo; cratonvm_jit::VARHANDLE_CAS_SLOTS] = [
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
const fn vh_cas_info(kind: usize) -> JitInvokeInfo {
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

/// Round 11 wave 18 (`r11w17-vh-varhandle-reference-slot-group-patch`): the
/// synthetic call site of every write/CAS slot is a site the slot function
/// itself binds to THAT slot, and the value-site classes the dispatch arms
/// from it fall in the slot's static-type group.
#[cfg(test)]
mod r11w18_vh_synthetic_site_groups {
    use super::*;
    use cratonvm_native_api::poly_call_site::{vh_site, vh_value_site_classes};

    #[test]
    fn every_write_info_binds_back_to_its_own_slot() {
        for (slot, info) in VARHANDLE_WRITE_INFOS.iter().enumerate() {
            assert_eq!(
                cratonvm_jit::varhandle_write_helper_slot(info.method_name, info.descriptor),
                Some(slot),
                "{} {}",
                info.method_name,
                info.descriptor,
            );
        }
        assert_eq!(
            varhandle_write_direct_fns().len(),
            VARHANDLE_WRITE_INFOS.len()
        );
    }

    #[test]
    fn every_cas_info_binds_back_to_its_own_slot() {
        for (slot, info) in VARHANDLE_CAS_INFOS.iter().enumerate() {
            assert_eq!(
                cratonvm_jit::varhandle_cas_helper_slot(info.method_name, info.descriptor),
                Some(slot),
                "{}",
                info.descriptor,
            );
        }
        assert_eq!(varhandle_cas_direct_fns().len(), VARHANDLE_CAS_INFOS.len());
    }

    /// `L` is armed as an `Object` site and `R` as an `OTHER` site, the
    /// class a primitive variable refuses statically (WMTE).
    #[test]
    fn the_reference_stand_ins_arm_their_own_group() {
        let r = cratonvm_jit::VARHANDLE_WRITE_KINDS.len() - 1;
        assert_eq!(cratonvm_jit::VARHANDLE_WRITE_KINDS[r], b'R');
        assert_eq!(cratonvm_jit::VARHANDLE_WRITE_KINDS[r - 1], b'L');
        assert_eq!(
            vh_value_site_classes(VARHANDLE_WRITE_DESCRIPTORS[r - 1]) & 0xff,
            u16::from(vh_site::OBJECT)
        );
        assert_eq!(
            vh_value_site_classes(VARHANDLE_WRITE_DESCRIPTORS[r]) & 0xff,
            u16::from(vh_site::OTHER)
        );
        let c = cratonvm_jit::VARHANDLE_CAS_KINDS.len() - 1;
        assert_eq!(cratonvm_jit::VARHANDLE_CAS_KINDS[c], b'R');
        let both = |class: u8| (u16::from(class) << 8) | u16::from(class);
        assert_eq!(
            vh_value_site_classes(VARHANDLE_CAS_DESCRIPTORS[c - 1]),
            both(vh_site::OBJECT)
        );
        assert_eq!(
            vh_value_site_classes(VARHANDLE_CAS_DESCRIPTORS[c]),
            both(vh_site::OTHER)
        );
        // The funnel's CAS parser still reads the stand-in as two references.
        assert_eq!(
            varhandle_cas_operand_kinds(VARHANDLE_CAS_DESCRIPTORS[c]),
            Some((b'L', b'L'))
        );
    }
}

/// Try to compile a callee method from a JitInvokeInfo.
/// Returns (entry_ptr, needs_context) if compilation succeeds.
// SAFETY: Caller must ensure vm is a valid SharedVm reference and info points to a live
// JitInvokeInfo. Delegates to try_jit_compile_callee which accesses the class manager
// and JIT compiler; no raw pointer dereferences occur within this function itself.
#[allow(clippy::type_complexity)]
unsafe fn try_compile_callee(
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
static INTEGER_VALUE_OF_INFO: JitInvokeInfo = JitInvokeInfo {
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
// What stood here until 2026-08-20 said these bodies need no internal JDK-only
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
// MEASURED, `--jdk-only`, binary at fe59bf9d9, `CRATONVM_INTRINSIC_STATS=1`, a
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
// A `--real-jdk` run of the same probe reports the identical `OSR 1`: the OSR
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
// switched off measured what `--jdk-only` measured with it on, i.e. the whole
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
/// on a stock `--jdk-only` run, which is why the admissions counter beside it
/// exists: without it the healthy state and "no compiled site ever got here"
/// are the same zero, which is the reading mistake [`jit_funnel_bypass_count`]'
/// doc describes for inert fast paths.
///
/// `vm`'s own count, summed over the per-thread tallies registered with its
/// `JitCache::jdk_only_sinks` (round 11: this was a process-global
/// `JIT_DIRECT_HELPER_JDK_ONLY_REFUSALS` static bumped per call; until t1 wave
/// 6, lane Y3, a sum over every thread's [`JitCounterBlock`] in the process,
/// so a VM's `--jdk-only-report` counted every other VM's calls too).
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
// The callee-side `--jdk-only` gate for the thin direct-call helpers.
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

/// One guarded thin direct-call helper's `--jdk-only` policy gate: the triple
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
    triple: DirectHelperTriple,
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
    capability: Option<cratonvm_native_api::CapabilityKind>,
    /// This gate's index into the calling VM's memo array,
    /// `JitRealm::direct_helper_gates` (assigned by `direct_helper_gates!`).
    ///
    /// nio21 wave 8 L2 (page
    /// `r21-direct-helper-gates-are-process-statics-holding-per-vm-memos-FIXED-20261009.md`):
    /// the memo itself, a `NativeCallSite` of this triple's resolution in ONE
    /// VM's `NativeMethodRegistry`, used to be a field here, i.e. per-VM
    /// state in a process static. It now lives in the VM's `JitRealm`, so the
    /// gate holds only `const` metadata and two VMs in one process (one
    /// `--jdk-only`, one `--compatible`) never share or thrash a memo.
    ordinal: usize,
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
enum DirectHelperTriple {
    /// The triple this helper's own decline edge dispatches to.
    Info(&'static JitInvokeInfo),
    /// The IMPLEMENTING row, when the call site names an interface.
    Implementation(&'static str, &'static str, &'static str),
}

impl DirectHelperGate {
    /// A gate for the triple this helper's own decline edge dispatches to.
    const fn for_info(info: &'static JitInvokeInfo, ordinal: usize) -> Self {
        Self {
            triple: DirectHelperTriple::Info(info),
            capability: None,
            ordinal,
        }
    }

    /// A gate for a triple that is NOT the one the call site names — H7-1's
    /// case. See [`DirectHelperTriple`].
    const fn for_triple(
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
    /// `docs/feature-designs/jdk-only-mode.md`'s
    /// second bullet.
    const fn for_info_capability(
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
    fn triple(&self) -> (&'static str, &'static str, &'static str) {
        match self.triple {
            DirectHelperTriple::Info(info) => (info.class_name, info.method_name, info.descriptor),
            DirectHelperTriple::Implementation(c, m, d) => (c, m, d),
        }
    }

    /// The class the capability field is an answer about.
    fn class_name(&self) -> &'static str {
        self.triple().0
    }

    /// The method the capability field is an answer about.
    fn method_name(&self) -> &'static str {
        self.triple().1
    }

    /// This gate's memo in `vm` (nio21 wave 8 L2): the `NativeCallSite` the
    /// VM's `JitRealm` holds at this gate's ordinal. Per VM, never shared.
    #[inline]
    fn site<'v>(&self, vm: &'v SharedVm) -> &'v cratonvm_native_api::NativeCallSite {
        vm.jit.direct_helper_gates.site(self.ordinal)
    }
}

/// Declare the guarded helpers' gates and, in the same breath, the enumeration
/// the tests sweep.
///
/// The enumeration is the reason this is a macro rather than twenty-odd plain
/// `static` items. `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
/// opens its "why it was not fixed with its sibling" with *eleven call sites,
/// not one*, and the hazard a per-site gate introduces is the same one
/// `NativeCallSite`'s own doc names: a gate shared between two sites, or a
/// gate whose triple is not the one its helper stands in front of, compiles
/// fine and is silently wrong. A list that is written by the declaration
/// cannot be short, so `every_direct_helper_gate_*` sweeps the population
/// rather than whichever gates a test author remembered.
///
/// nio21 wave 8 L2: the macro also assigns each gate its ordinal (the
/// position of its variant in `DirectHelperGateOrdinal`) and the count
/// [`DIRECT_HELPER_GATE_COUNT`], which sizes the per-VM memo array
/// `JitRealm::direct_helper_gates`. The `static`s keep only `const` metadata.
macro_rules! direct_helper_gates {
    ($(
        $(#[$meta:meta])*
        $gate:ident = $src:tt;
    )*) => {
        /// One variant per gate, in declaration order: the gate's index into
        /// the VM's memo array. Never constructed; only cast.
        #[allow(non_camel_case_types, dead_code, clippy::upper_case_acronyms)]
        #[repr(usize)]
        enum DirectHelperGateOrdinal {
            $($gate,)*
            Count,
        }

        /// How many gates `direct_helper_gates!` declared: the length of the
        /// per-VM memo array (`JitRealm::direct_helper_gates`).
        pub(crate) const DIRECT_HELPER_GATE_COUNT: usize = DirectHelperGateOrdinal::Count as usize;

        $(
            $(#[$meta])*
            static $gate: DirectHelperGate =
                direct_helper_gates!(@build $src, DirectHelperGateOrdinal::$gate as usize);
        )*

        /// Every gate declared above, with its item name, for the sweeps.
        #[cfg(test)]
        const ALL_DIRECT_HELPER_GATES: &[(&str, &DirectHelperGate)] =
            &[$((stringify!($gate), &$gate)),*];
    };
    (@build (info: $info:ident), $ord:expr) => { DirectHelperGate::for_info(&$info, $ord) };
    (@build (info: $info:ident, capability: $cap:expr), $ord:expr) => {
        DirectHelperGate::for_info_capability(&$info, $cap, $ord)
    };
    (@build (triple: $c:expr, $m:expr, $d:expr), $ord:expr) => {
        DirectHelperGate::for_triple($c, $m, $d, $ord)
    };
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
    /// OSR door binding under `--jdk-only` (298 000 calls, `OSR 1`).
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
    THREAD_CURRENT_CARRIER_THREAD_GATE = (info: THREAD_CURRENT_CARRIER_THREAD_INFO);
}

/// Whether the thin direct-call helper behind `gate` must decline its fast
/// path for `vm` and route to the generic dispatch helper.
///
/// The callee-side half of the JDK-only direct-helper gate — see the module
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
fn jit_direct_helper_refused(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    // A capability-classified gate is checked in EVERY mode: the funnel's
    // `check_native_dispatch_capability` runs regardless of `--jdk-only` /
    // `--compatible`, and `--capabilities` is the switch that makes it
    // matter, not the class-library policy. Only the `Unsafe` gates are
    // classified; for every other gate this is the early return it always was.
    //
    // Outside `--jdk-only` a classified gate asks the shared decision directly
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
/// (`docs/feature-designs/jdk-only-mode.md`).
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
/// `--jdk-only` refuses the helpers through `direct_helper_gate_refuses`).
/// Refused, the helper takes its own `break 'fast` route to
/// `jit_invoke_dispatch`, which runs the JDK body. Correctness, unconditional
/// (`CRATONVM_COMPAT_ALIGN=-jdk-hash-collections` restores the stand-in maps
/// and with them the helpers' fast paths).
#[inline]
fn hashmap_gate_under_jdk_model(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
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
fn compat_row_refuses_collection_bind(
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
fn chm_gate_under_jdk_model(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    std::ptr::eq(gate, &CONCURRENT_HASHMAP_GET_GATE)
        && vm
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
}

/// The `JdkOnly` (and capability-classified) arm of
/// [`jit_direct_helper_refused`].
///
/// NOT cold, whatever this used to say: `--jdk-only` is the launcher default,
/// so under it every call into a guarded helper arrives here, and the
/// `Unsafe` gates arrive here in every mode. Kept out of line so the
/// `Compatible` early return stays small; the tally is per-thread (see
/// [`JitCounterBlock`]) because this runs per call.
#[inline(never)]
fn direct_helper_gate_refuses(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
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
fn direct_helper_tail_would_refuse(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    let (class_name, method_name, descriptor) = gate.triple();
    let registry = &vm.natives.native_methods;
    let id = gate
        .site(vm)
        .resolve(registry, class_name, method_name, descriptor);
    let kind = id.and_then(|id| registry.kind_of_id(id));
    // JIT round 23 wave 8 (lane W8-L2, CF-W8-1): a CLASSIFIED gate (the
    // `Unsafe` CAS / accessor family) under an installed policy -- every
    // booted VM -- never reached the memo below, so under `--jdk-only` each
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
fn gate_memo_policy_admits(gate_is_classified: bool, policy_installed: bool) -> bool {
    !gate_is_classified || !policy_installed
}

/// `CRATONVM_JIT_GATE_POLICY_HALF_MEMO` (JIT round 23 wave 8, lane W8-L2,
/// CF-W8-1; perf, default ON, `=0` restores the per-call resolver question):
/// [`classified_gate_refuses_split`]. Read once per process, as its sibling
/// [`gate_decision_memo_enabled`] is.
fn gate_policy_half_memo_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_GATE_POLICY_HALF_MEMO")
    })
}

/// [`direct_helper_tail_would_refuse`] for a capability-classified gate under
/// an installed policy, split in its two halves.
///
/// The capability half is asked on EVERY call, through the funnel's own
/// function, so `Audit` / `Permissive` record exactly as before and `Enforce`
/// can still deny. Only past it, and only under `--jdk-only` (the
/// `--compatible` answer is "admit", as in [`direct_helper_refusal`]), is the
/// policy half -- [`direct_helper_refusal`] with no capability question --
/// served from the gate's memo cell, under the same key and the same
/// settledness rule an unclassified gate's whole answer uses. The cell is
/// never filled for this gate by the unsplit path while a policy is installed
/// ([`gate_memo_policy_admits`]), and without one the two answers coincide
/// (an absent policy admits every capability question).
fn classified_gate_refuses_split(
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
fn gate_memo_key(vm: &SharedVm) -> (u32, u32) {
    (
        vm.natives.native_methods.generation(),
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
fn thin_helper_answer_settled(
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
struct GateDecisionMemo {
    vm: usize,
    kind: u8,
    /// COMPAT21-2: `gate_memo_key`'s registry generation at the fill.
    generation: u32,
    /// COMPAT21-2: `gate_memo_key`'s redefine epoch at the fill.
    redefine_epoch: u32,
    refused: bool,
}

thread_local! {
    static GATE_DECISION_MEMO: [Cell<Option<GateDecisionMemo>>; DIRECT_HELPER_GATE_COUNT] =
        const { [const { Cell::new(None) }; DIRECT_HELPER_GATE_COUNT] };
}

/// `CRATONVM_JIT_GATE_DECISION_MEMO` (default ON, `=0` asks the full
/// decision on every call). Read once per process: both arms answer the same.
fn gate_decision_memo_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_GATE_DECISION_MEMO")
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
enum CapabilityQuestion {
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
fn thin_helper_capability_admits(
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

// ===========================================================================
// H20-1 — the bind-time half: one enumerated address source for every compile
// door, and a `match` the compiler will not let anyone leave incomplete.
//
// # Why the enforcement is here and not on `JitDirectCall`
//
// `H12-1` N1 asked for `CompileAdmission`'s trick to be applied to
// `JitDirectCall`: make it un-constructible outside a constructor that takes a
// policy witness. The analogy is right about the *problem* and wrong about the
// *place*, for a reason that is a property of the backend rather than of taste.
//
// `CompileAdmission` works because it is consumed at a choke point DOWNSTREAM
// of every door (`x64::compile_with_param_slots`) and the refusal is safe
// there: the caller's fallback — "do not compile, keep interpreting" — is
// universal and every door already handles it.
//
// A direct-bind refusal has no such downstream point. `x64/driver.rs`'s
// `reserve_stack_floor` walk states the backend's rule in its own words: *"a
// raw self-call site is an `invokestatic` pc with neither an invoke-info entry
// nor a direct-call plan"*. Every ladder — this crate's two and
// `try_compile_inner`'s two — pushes its row and then `continue`s PAST the
// `invoke_info` construction for that pc (`jit_bridge.rs`'s pushes are at
// 1243/1371/1435, all after the ladder). So a row deleted after the door
// leaves the pc with no metadata at all, and the `0xb8` arm compiles
// `Thread.currentThread()` as a call to the enclosing method. Filtering
// downstream would replace an open door with a wild jump.
//
// `try_compile_inner` already has the only safe shape, and it is the model:
//
//     let entry = direct_native_helper(..);      // 0 == refused
//     if entry != 0 { direct_calls.push(..); continue; }
//     // falls through to the code that builds `invoke_info` for this pc
//
// So the token has to ride on the ENTRY ADDRESS, obtained at the bind site,
// before the `continue`. That is what [`admit_direct_native_entry`] is:
// `Option<usize>`, `None` meaning "fall through", which is byte-for-byte the
// `entry == 0` sentinel the MethodEntry ladder has always used.
//
// # What is compiler-enforced, and what is not
//
// Enforced by this file:
//   * a thirty-second thin helper cannot be added without stating the
//     registered triple it stands in front of — `direct_native_shadows!`
//     takes the triple and the symbol together and writes all three matches
//     from them, so the arms cannot drift and none can be left out;
//   * a door cannot ask for an address without naming which VM it is
//     compiling for and which admission it holds, because those are the only
//     parameters;
//   * **a ladder outside `crate::jit` cannot name a helper item at all.** The
//     thirty-one items are `pub(in crate::jit)` as of 2026-09-22, so
//     `helpers::jit_hashmap_get_direct as *const () as usize` in
//     `vm/src/runtime/interpreter/jit_bridge.rs` is a compile error rather
//     than a bypass a reviewer has to notice.
//
// Enforced by `jit::compile_gate`:
//   * a fourth `CompileDoor` cannot be added without answering
//     `builds_direct_calls`;
//   * a door that never declares a `DirectCallPolicy` has its rows counted by
//     `undeclared_direct_bind_rows`, which all three doors now read zero for
//     and `vm-cli/tests/jit_compile_gate_doors.rs` asserts zero.
//
// This paragraph used to end: *"NOT enforced, and named so it is not mistaken
// for enforced: the raw items below are still `pub` … which is what both
// direct doors do today. Making them `pub(in crate::jit)` is what closes it …
// Lane H20 owns neither and was forbidden to build; H20-1 §6 O1/O2 specify the
// change."* That is what landed on 2026-09-22.
//
// NOT enforced, and named so it is not mistaken for enforced: the eight
// `jdk/internal/misc/Unsafe` scalar accessors are deliberately not
// [`DirectNativeShadow`] variants and are still `pub`, still bound raw at the
// OSR and single-pass doors. `DirectNativeShadow`'s own doc says why, and the
// capability bypass that decision leaves open is tracked at
// `docs/feature-designs/jdk-only-mode.md`.
// `direct_bind_source_witness` below is what notices a helper item widened
// back to `pub` on the way to a thirty-second door.
// ===========================================================================

/// A thin direct-call helper that stands in front of a **registered native**,
/// enumerated so the policy question can be asked once per helper instead of
/// once per door.
///
/// Membership rule, and it is narrower than "everything in `direct_calls`":
/// a variant belongs here iff the emitted `CALL` reaches a VM-side body that
/// re-implements a row in the native registry **and that row is one §7 step 3
/// has something to say about**. That excludes
/// `Math.sqrt` (lowered to `SQRTSD`, shadows nothing), the `String`/`CRC32`
/// call-site intrinsics and the `JitIntrinsic` sentinels, and the
/// `AtomicInteger` family (resolved by `try_resolve_atomic_intrinsic`, which
/// carries its own admission). Those owe no policy question and listing them
/// here would invite a "fix" that is not one — `H12-2` §1's last bullet.
///
/// # 2026-09-22: 9 → 31, and the two lists this is still not
///
/// It was nine when only the nine helpers `H12-2` had measured were spelled,
/// and it stayed nine while twenty-two more thin helpers were added at the
/// doors — which is the same drift `DIRECT_CALL_HELPER_NATIVES` had and for
/// the same reason. `H12-1` O1/O2 asked the OSR and eager-first-call ladders
/// to obtain their addresses through [`admit_direct_native_entry`]; a ladder
/// cannot do that for a helper with no variant, so the population had to
/// become the whole population first.
///
/// **Not `DIRECT_CALL_HELPER_NATIVES`.** That list answers "is this registry
/// row's `invocations` column a floor?" and so carries a row per REGISTRY
/// SLOT: both `ConcurrentMap.get` and `ConcurrentHashMap.get` are on it
/// because either may be the slot a fallback resolves. This one carries a
/// variant per HELPER and names the implementing row only — H7-1's rule, which
/// [`DirectNativeShadow::triple`] restates below.
///
/// **Not the eight `Unsafe` scalar accessors.** `jdk/internal/misc/Unsafe`
/// natives have no bytecode, so §1.4 and §7 step 3 have nothing to prefer over
/// them and `direct_helper_refusal` would admit every one — except that
/// `capability::classify_native` maps the WHOLE class to
/// `CapabilityKind::RawMemory`, so the capability rule would refuse all eight
/// instead. Refusing them here would apply that gate at ONE door of three (the
/// single-pass ladder reads `UnsafeAccessorOp::helper_entry` out of the
/// `DirectHelperTable` and cannot call this function), which is `H12-1`'s own
/// defect with the modes swapped. The bypass is real and is not this
/// enumeration's to close:
/// `docs/feature-designs/jdk-only-mode.md`.
macro_rules! direct_native_shadows {
    ($(
        $(#[$vmeta:meta])*
        $variant:ident = $symbol:ident, ($class:literal, $method:literal, $desc:literal);
    )*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum DirectNativeShadow {
            $( $(#[$vmeta])* $variant, )*
        }

        impl DirectNativeShadow {
            /// Every shadow, for the doors' sweeps and the tests.
            ///
            /// A slice and not an array: the length is a property of the
            /// invocation below, and a hand-written one is one more thing that
            /// can be wrong about it. It went 9 → 31 in a single change that
            /// would have had to touch the number four times.
            pub const ALL: &'static [DirectNativeShadow] = &[
                $( DirectNativeShadow::$variant, )*
            ];

            /// The registered triple whose native this helper re-implements.
            ///
            /// For `ConcurrentHashMapGet` this is the **implementing** class,
            /// not the `java/util/concurrent/ConcurrentMap` interface the call
            /// site names — H7-1's finding, and the reason
            /// `direct_native_helper_for_impl` exists. A policy question asked
            /// about the interface row is a question about a registration that
            /// does not run.
            pub const fn triple(self) -> (&'static str, &'static str, &'static str) {
                match self {
                    $( DirectNativeShadow::$variant => ($class, $method, $desc), )*
                }
            }

            /// The Rust item name, so a refusal and the source witness can
            /// both name the same symbol the ladders name.
            ///
            /// Test-only: every caller is in this file's `#[cfg(test)]`
            /// module. Nothing in production asks a shadow for its symbol
            /// NAME — the doors take `entry_addr()`, which since `H12-1`
            /// O1/O2 is the only way they can name one at all.
            #[cfg(test)]
            pub const fn helper_symbol(self) -> &'static str {
                match self {
                    $( DirectNativeShadow::$variant => stringify!($symbol), )*
                }
            }

            /// The helper's entry address.
            ///
            /// Taken as `NAME as *const () as usize` rather than read out of
            /// the `*_DIRECT_FN` cell, for the reason both direct doors used
            /// to state at every arm: a door can run before `build_helpers`
            /// has published those cells, so a cell read yields `0` on the
            /// first compile in a process and the bind is silently skipped.
            /// Process-invariant either way — the cells hold exactly these
            /// addresses (`build_helpers`, registered unconditionally since
            /// 2026-08-06).
            ///
            /// Private on purpose, and since 2026-09-22 the items it names are
            /// `pub(in crate::jit)` as well, so a ladder outside this module
            /// cannot reach one without [`admit_direct_native_entry`]. That is
            /// `H12-1` N1's ask, landed at the address rather than at
            /// `JitDirectCall` — see the module block above for why the token
            /// has to ride there.
            fn entry_addr(self) -> usize {
                match self {
                    $( DirectNativeShadow::$variant => $symbol as *const () as usize, )*
                }
            }
        }
    };
}

direct_native_shadows! {
    /// `java/lang/Thread.currentThread()` — the helper `H12-1` MEASURED the
    /// OSR door binding under `--jdk-only`: 298 000 compiled calls while the
    /// MethodEntry door refused all seven sites it examined.
    ThreadCurrentThread = jit_thread_current_thread_direct,
        ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;");
    /// `java/lang/ThreadLocal.get()` (SBR round 2, wave 8, lane `tlocal`): a reviewed
    /// `Intrinsic` under `--jdk-only`, served from the real `Thread.threadLocals`
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
    ThreadCurrentCarrierThread = jit_thread_current_carrier_thread_direct,
        ("java/lang/Thread", "currentCarrierThread", "()Ljava/lang/Thread;");
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
    // Checked BEFORE the `--jdk-only` early return, and unconditionally of
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
pub(crate) fn compat_bind_asks_registry_enabled(vm: &SharedVm) -> bool {
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
/// `docs/feature-designs/jdk-only-mode.md`).
/// It is `--jdk-only`'s `None => refuse` arm of [`direct_helper_refusal`], and
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
/// question `--jdk-only` already asks ([`thin_helper_tail_yields_to_bytecode`]),
/// at bind time only: a refused site is never bound, so the call-time gate
/// ([`jit_direct_helper_refused`]) has nothing to disagree with, and the hot
/// `--compatible` early return there is unchanged. A refused site keeps its
/// ordinary ladder (dispatch, IC or splice of the JDK body) -- what
/// `--jdk-only`, where those stub rows are not registered at all, runs today.
/// An `Intrinsic` row is never refused (§1.4's reviewed exception).
///
/// Compile-time only: one registry hash plus, for a `Bridge` / stub row, the
/// resolver's loaded-declaration lookup, per recognised site per compile.
fn compat_bind_yields_to_bytecode(
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

fn compat_bind_refuses_unregistered(
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
fn compat_site_row_inherited_or_unknown(
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
///   deny it". It is asked in EVERY mode, before the `--jdk-only` question,
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
/// refused call in `--jdk-only-report`'s observation sinks, once for the
/// question and once for the answer.
///
/// Callers must have established `JdkOnly` first for an UNCLASSIFIED triple;
/// this does not re-test it, because both do so to reach their own cold arm.
/// A classified triple reaches here in every mode, so its arm re-tests.
fn direct_helper_refusal(
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
        // is the `--jdk-only` policy, which an unclassified gate never asks
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
        // §1.3 — a fake may not be invoked under `--jdk-only`. The tail turns
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
fn thin_helper_asks_resolver_enabled(vm: &SharedVm) -> bool {
    vm.jit_thin_helper_asks_resolver
}

/// Would `vm_exec::invoke_or_native` -- the tail a thin helper's decline
/// routes to -- yield `(class_name, method_name, descriptor)`'s registered
/// native of `kind` to the loaded declaration's bytecode? Behind
/// [`thin_helper_asks_resolver_enabled`]; the decision is
/// [`thin_helper_resolver_yields`].
fn thin_helper_tail_yields_to_bytecode(
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
fn thin_helper_resolver_yields(
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
/// counter and the same `--jdk-only-report` violation list the MethodEntry
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
/// account of what it does is the thing that was wrong: `--jdk-only` and
/// `--real-jdk` measured an identical bind profile while one of them was
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
    // otherwise admit. Neither is a `--jdk-only` violation, so neither is
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
unsafe fn jit_indy_bridge_body(
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
    if vm_ptr != 0 && arg_count >= 0 {
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
    let kind = crate::runtime::invokedynamic::jit_indy_site_kind(site_ptr as usize);
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
unsafe fn jit_indy_leaf_dispatch(
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
fn jit_box_cold_alloc(
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
    if !latched || zgc_jit_box_collects_latched_enabled() {
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

/// zs s6/no: `CRATONVM_ZGC_JIT_BOX_COLLECTS_LATCHED` (default ON, `=0` kills):
/// see the latched arm of [`jit_box_cold_alloc`]. Read only after the box's
/// fallible door refused on a heap whose GC-overhead streak is latched.
fn zgc_jit_box_collects_latched_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JIT_BOX_COLLECTS_LATCHED")
}

#[cfg(test)]
mod zr7_rv6_box_owed_tests {
    //! zr z7/rv6: the JIT box door after a futile-skipped refusal.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_gc::heap::ArrayElementType;
    use std::sync::atomic::Ordering;

    /// A full ZGC heap collected with every byte pinned live (nothing
    /// allocated since: the futile window), whose pins the program then
    /// DROPS. A compiled `Integer.valueOf` box allocation must collect and
    /// succeed, as HotSpot's does. Before the fix the cr rule skipped the
    /// door's forced collection (window < 4 KiB) and the door, which has no
    /// clear-soft rung to pay it, fell through to the OOME.
    #[test]
    fn a_box_after_a_futile_skip_pays_the_owed_collection() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            max_heap_size: 16 << 20,
            initial_heap_size: 16 << 20,
            ..VmConfig::default()
        });
        if !matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Zgc(_)) {
            return; // a build without the `zgc` feature
        }
        let mut thread = JvmThread::new(ThreadId(0), "zr7rv6-box");
        let heap = &shared.mem.heap;
        let fill = |thread: &mut JvmThread| -> usize {
            let mut n = 0usize;
            for len in [64 * 1024usize, 4096, 256, 16, 1] {
                while let Some(a) =
                    heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, len)
                {
                    thread.native_pin_roots.push(a);
                    n += 1;
                }
            }
            while let Some(o) = heap.try_alloc_object_full(ClassId::new(77), 1) {
                thread.native_pin_roots.push(o);
                n += 1;
            }
            n
        };
        let mut full = false;
        for _ in 0..16 {
            fill(&mut thread);
            crate::runtime::interpreter::maybe_gc_forced_pub_at(
                &shared,
                &mut thread,
                "zr7rv6-fill",
            );
            if fill(&mut thread) == 0 {
                full = true;
                break;
            }
        }
        assert!(full, "the pins never filled the heap");
        let window = heap.zgc_bytes_allocated_since_cycle().expect("ZGC");
        assert!(
            window < crate::runtime::interpreter::gc_and_alloc::ZGC_REFUSAL_FUTILE_WINDOW,
            "fixture: window {window}"
        );
        // The program drops everything; no collection has seen it yet.
        thread.native_pin_roots.clear();
        shared
            .mem
            .gc_unproductive_streak
            .store(0, Ordering::Relaxed);
        let cycles = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let boxed = jit_box_cold_alloc(&shared, &mut thread, ClassId::new(77));
        assert!(boxed.is_some(), "a heap of dropped garbage refused one box");
        assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles);
        assert!(!shared
            .mem
            .tlab_wedge
            .zgc_refusal
            .soft_rung_owed
            .load(Ordering::Relaxed));
    }

    /// zs s6/no: the same dropped heap with the GC-overhead streak LATCHED
    /// (the state a fill-to-OOME leaves). The box door used to skip its
    /// collection there and fall through to the OOME; now it collects as the
    /// interpreter's ladder does. `CRATONVM_ZGC_JIT_BOX_COLLECTS_LATCHED=0`
    /// is the positive control: the old skip, no collection, no box.
    #[test]
    fn a_box_under_a_latched_streak_still_collects_a_dropped_heap() {
        for (kill, served) in [(None, true), (Some("0"), false)] {
            let _arm = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_ZGC_JIT_BOX_COLLECTS_LATCHED",
                    kill,
                )]),
            );
            let shared = SharedVm::new(VmConfig {
                gc_algorithm: GcAlgorithm::Zgc,
                max_heap_size: 16 << 20,
                initial_heap_size: 16 << 20,
                ..VmConfig::default()
            });
            if !matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Zgc(_)) {
                return; // a build without the `zgc` feature
            }
            let limit =
                crate::runtime::interpreter::gc_and_alloc::gc_overhead_limit_cycles(&shared);
            assert!(limit > 0, "fixture: the default VM has an overhead limit");
            let mut thread = JvmThread::new(ThreadId(0), "zs6no-box");
            let heap = &shared.mem.heap;
            let fill = |thread: &mut JvmThread| -> usize {
                let mut n = 0usize;
                for len in [64 * 1024usize, 4096, 256, 16, 1] {
                    while let Some(a) =
                        heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, len)
                    {
                        thread.native_pin_roots.push(a);
                        n += 1;
                    }
                }
                while let Some(o) = heap.try_alloc_object_full(ClassId::new(77), 1) {
                    thread.native_pin_roots.push(o);
                    n += 1;
                }
                n
            };
            let mut full = false;
            for _ in 0..16 {
                fill(&mut thread);
                crate::runtime::interpreter::maybe_gc_forced_pub_at(
                    &shared,
                    &mut thread,
                    "zs6no-fill",
                );
                if fill(&mut thread) == 0 {
                    full = true;
                    break;
                }
            }
            assert!(full, "the pins never filled the heap");
            thread.native_pin_roots.clear();
            shared
                .mem
                .gc_unproductive_streak
                .store(limit, Ordering::Relaxed);
            assert!(crate::runtime::interpreter::gc_overhead_limit_exceeded(
                &shared
            ));
            let cycles = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
            let boxed = jit_box_cold_alloc(&shared, &mut thread, ClassId::new(77));
            assert_eq!(boxed.is_some(), served, "kill {kill:?}");
            assert_eq!(
                shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles,
                served,
                "kill {kill:?}"
            );
        }
    }
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
unsafe fn jit_integer_value_of_direct_body(vm_ptr: i64, value: i64) -> i64 {
    jit_integer_value_of_direct_body_on(vm_ptr, value, None)
}

/// The box helpers' entry bookkeeping. From compiled code (`held == false`):
/// [`jit_safepoint_flush_satb`], whose `native_pending_return` drain takes the
/// JIT thread. From a caller that already HOLDS that thread (`_on` with
/// `Some`, the lambda arm; r23 w3 review): the SATB flush alone -- the drain's
/// `jit_thread_mut` would be the sibling borrow the `_on` split exists to
/// avoid (a debug-build `JIT_THREAD_BORROWED` trip, aliasing `&mut` in
/// release), and the funnel this replaces never drained there either.
unsafe fn jit_box_helper_entry_flush(vm_ptr: i64, held: bool) {
    if !held {
        jit_safepoint_flush_satb(vm_ptr);
    } else if vm_ptr != 0 {
        // SAFETY: as `jit_safepoint_flush_satb`: a live SharedVm pointer.
        let vm = &*(vm_ptr as *const SharedVm);
        vm.mem.heap.flush_thread_satb();
    }
}

/// [`jit_integer_value_of_direct_body`] on a thread the caller already HOLDS
/// (`given`), JIT round 23 wave 3b, lane L2: the compiled lambda arm boxes an
/// adapted site's `int` return here (`lambda_site_adapt_return`,
/// `CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX`) instead of through the native funnel,
/// and it already holds the JIT thread -- a second `jit_thread_mut` borrow from
/// inside it would be a sibling borrow. `None` takes the thread as before. The
/// semantics are the helper's: the box cache (`-128..=IntegerCache.high`)
/// answers its own objects, a value past it is a fresh TLAB object, and a cold
/// slot or an unresolved bound takes the canonical native.
unsafe fn jit_integer_value_of_direct_body_on(
    vm_ptr: i64,
    value: i64,
    mut given: Option<&mut JvmThread>,
) -> i64 {
    // Same Rust<->JIT boundary bookkeeping as `jit_invoke_dispatch`: the
    // per-thread conservative-scan cache must be invalidated, and a callee
    // that allocates may enter the GC barrier, so the SATB queue is flushed.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_box_helper_entry_flush(vm_ptr, given.is_some());
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let value = value as i32;
    // Outside the box cache, where `valueOf` must return a FRESH object. The
    // cache's upper end is `-Djava.lang.Integer.IntegerCache.high`, not 127:
    // this helper used to allocate for everything past 127, so with `high=1000`
    // `Integer.valueOf(500) == Integer.valueOf(500)` was true interpreted and
    // false once the caller compiled. Until the native has resolved this VM's
    // bound, every value above 127 takes the native path, which resolves it.
    let beyond_cache = value < -128
        || cratonvm_native_builtins::lang_math::integer_cache_high_for(vm.vm_identity)
            .is_some_and(|high| value > high);
    if beyond_cache {
        let vm_key = vm.vm_identity;
        let cached_class = INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
            cache
                .get()
                .filter(|(cached_vm, _)| *cached_vm == vm_key)
                .map(|(_, raw)| ClassId::new(raw))
                .filter(|class_id| !class_was_redefined(vm, *class_id))
        });
        if let Some(class_id) = cached_class {
            // Wave 3b: the caller's thread when it holds one (see `_on`).
            let mut _jit_borrow = None;
            let held: Option<&mut JvmThread> = match given.as_deref_mut() {
                Some(t) => Some(t),
                None => jit_thread_mut().map(|(t, g)| {
                    _jit_borrow = Some(g);
                    t
                }),
            };
            if let Some(thread) = held {
                // Direct TLAB bump — the SAME allocation function
                // `NativeContextImpl::alloc_object`'s TLAB arm uses (full
                // header init, fresh identity hash), minus that method's
                // per-call clamp-cache scan, alloc-pool probes and context
                // plumbing. The slot count is the class's real declared
                // field count, resolved once per (vm, class) below — so the
                // undersized-layout clamp is honored, not skipped. TLAB
                // exhaustion (or an oversized layout) falls back to the full
                // allocator with its old-gen spill/batch behavior.
                thread_local! {
                    // (vm_key, class_id, slots). The exact-class redefine gate
                    // above prevents use after that class's layout changes.
                    static INTEGER_ALLOC_SLOTS: std::cell::Cell<Option<(usize, u32, u32)>> =
                        const { std::cell::Cell::new(None) };
                }
                let slots = INTEGER_ALLOC_SLOTS.with(|cache| {
                    if let Some((vk, cid, slots)) = cache.get() {
                        if vk == vm_key && cid == class_id.as_u32() {
                            return slots as usize;
                        }
                    }
                    let resolved = vm
                        .classes
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.num_total_fields.max(1))
                        .unwrap_or(1);
                    // Cast: field counts are far below u32::MAX.
                    cache.set(Some((vm_key, class_id.as_u32(), resolved as u32)));
                    resolved
                });
                // The shape planner, not a bare legacy size: every TLAB object
                // site has to agree with the header its allocation will be
                // stamped with. See `plan_tlab_object_shape`.
                //
                // r9w12: memoized per `(vm_key, class_id, slots)`, because for
                // a fixed triple the planner is a pure function and this arm
                // called it on EVERY escaping box. Its
                // `compact_tlab_body_size` is a `with_current_class_layout`
                // lookup — 3.65 % of `BoxProbe box` on the 2026-09-20 dev
                // build, on top of the 14.88 % this whole helper cost.
                //
                // The key carries `class_id`, which the `class_was_redefined`
                // screen above has already rejected if this class's layout
                // moved, so a stale plan cannot be served across a
                // redefinition. It is keyed on `slots` as well because that is
                // the planner's second argument, not because it can drift
                // under a fixed class.
                //
                // `tlab_shape_planner_is_instrumented` is the one thing that
                // makes the planner impure: the shape counters and
                // `CRATONVM_DBG_COMPACT_TLAB`. While either is armed this
                // re-plans per call, so a memo never blinds an instrument. See
                // that predicate.
                thread_local! {
                    // (vm_key, class_id, slots, requested_size)
                    static INTEGER_ALLOC_PLAN: std::cell::Cell<
                        Option<(usize, u32, u32, u32)>,
                    > = const { std::cell::Cell::new(None) };
                }
                let planner_instrumented =
                    crate::runtime::interpreter::tlab_shape_planner_is_instrumented();
                let memo_key = (vm_key, class_id.as_u32(), slots as u32);
                let cached_plan = (!planner_instrumented)
                    .then(|| INTEGER_ALLOC_PLAN.with(|c| c.get()))
                    .flatten()
                    .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                    .map(|(_, _, _, size)| size as usize);
                let requested_size = match cached_plan {
                    Some(size) => size,
                    None => {
                        let (size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
                            vm,
                            class_id,
                            slots,
                            crate::runtime::interpreter::tlab_site::JIT_HELPER,
                        );
                        if !planner_instrumented {
                            if let Ok(size_u32) = u32::try_from(size) {
                                INTEGER_ALLOC_PLAN.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, size_u32)))
                                });
                            }
                        }
                        size
                    }
                };
                let tlab_object = if requested_size <= cratonvm_gc::tlab::tlab_max_alloc() {
                    crate::runtime::interpreter::tlab_alloc_object(
                        thread,
                        vm,
                        class_id,
                        slots,
                        requested_size,
                    )
                } else {
                    None
                };
                if let Some(object) = tlab_object {
                    // Raw primitive-cell write: this arm JUST allocated
                    // `object` through the legacy TLAB path
                    // (`init_object_header`, zeroed 16-byte Value cells), so
                    // field 0 is the Value cell at HEADER_SIZE — the exact
                    // bytes `set_field_as(.., b'I')` would store, minus that
                    // path's per-call header read + layout dispatch. A
                    // primitive store takes no write barrier.
                    // ...but ONLY when it really is legacy. That premise was
                    // an assumption about which allocator this site calls, not
                    // a property of the object, and `CRATONVM_COMPACT_TLAB_ALLOC`
                    // falsified it: with compact planning on, `java.lang.Integer`
                    // gets a packed 4-byte `value` and this 16-byte `Value` cell
                    // overwrote it and the bytes after it. `FjpProbe` summed
                    // boxed integers and returned 215812748544 instead of
                    // 499999500000 -- no collection involved, exactly as the
                    // note on `tlab_alloc_object_inner` described. The cold arm
                    // immediately below has always used the layout-aware store
                    // and says why; this asks the object rather than assuming.
                    // SAFETY: `object` was just allocated here, so its first
                    // HEADER_SIZE bytes are a live, fully written `ObjectHeader`.
                    let header =
                        unsafe { &*(object.as_ptr() as *const cratonvm_types::ObjectHeader) };
                    let compact = cratonvm_types::is_compact_object(header);
                    if compact {
                        // r9w12: the packed `value` cell, memoized like the
                        // shape plan above and for the same reason.
                        //
                        // `set_field_as(.., 0, Int(v), b'I')` is three lookups
                        // for one 4-byte store: `coerce_field_value_for_slot`
                        // (which reads `class_id_of(obj)` through the checked
                        // heap API), then `set_field`'s SATB arm, then
                        // `set_field_no_card`'s `zgc_compact_field_storage` --
                        // a THIRD `with_current_class_layout` walk, after the
                        // planner's and the header's. On the 2026-09-20 dev
                        // build `BoxProbe box` spent 3.04 % in
                        // `coerce_field_value_for_slot`, 1.30 % in
                        // `set_field_as`, 1.54 % in `set_field_no_card` and
                        // 1.27 % in `CurrentLayoutCache::find`.
                        //
                        // A `CompactLayout` belongs to a `(class_id,
                        // field_count)` pair, not to an instance, so the
                        // `(offset, storage)` of field 0 is the same for every
                        // object this arm allocates until that class is
                        // redefined -- which the `class_was_redefined` screen
                        // at the top of this arm already refuses to serve past.
                        //
                        // Restricted to a 4-byte `Int` cell, which is what
                        // `java.lang.Integer.value` is. Anything else (a
                        // reference-typed slot 0, a narrowed width, no
                        // registered layout) declines to `set_field_as`, so the
                        // coercion, the boxing arm and the
                        // HIB-DCAST-LATEPHASE.1 guard all stay reachable for
                        // every shape that is not exactly this one.
                        thread_local! {
                            // (vm_key, class_id, slots, offset)
                            static INTEGER_VALUE_CELL: std::cell::Cell<
                                Option<(usize, u32, u32, u32)>,
                            > = const { std::cell::Cell::new(None) };
                        }
                        let offset = INTEGER_VALUE_CELL
                            .with(|c| c.get())
                            .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                            .map(|(_, _, _, offset)| offset as usize)
                            .or_else(|| {
                                let (offset, storage) =
                                    cratonvm_types::compact_object_field_disp(header, 0)?;
                                if storage != cratonvm_types::FieldStorageKind::Int {
                                    return None;
                                }
                                let offset_u32 = u32::try_from(offset).ok()?;
                                INTEGER_VALUE_CELL.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, offset_u32)))
                                });
                                Some(offset)
                            });
                        match offset {
                            // SAFETY: `object` is this arm's own just-allocated
                            // compact body, and `offset` is field 0's byte
                            // offset in the layout THIS object's header names,
                            // whose storage was checked to be a naturally
                            // aligned 4-byte `Int` cell. A primitive store into
                            // a fresh object takes no write barrier -- the same
                            // argument the legacy arm below makes.
                            Some(offset) => unsafe {
                                cratonvm_types::write_compact_field(
                                    object.as_ptr().add(offset),
                                    cratonvm_types::FieldStorageKind::Int,
                                    Value::Int(value),
                                    std::sync::atomic::Ordering::Relaxed,
                                )
                            },
                            None => vm.mem.heap.set_field_as(object, 0, Value::Int(value), b'I'),
                        }
                    } else {
                        // SAFETY: `object` is a live legacy-layout allocation
                        // with >= 1 slot (`slots.max(1)` above) -- now CHECKED
                        // immediately above, not assumed; the cell is
                        // exclusively ours until published below.
                        unsafe {
                            std::ptr::write(
                                object.as_ptr().add(cratonvm_gc::heap::HEADER_SIZE) as *mut Value,
                                Value::Int(value),
                            );
                        }
                    }
                    // Object-return handoff root (see `call_integer_native_raw`).
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                // zr z3/lt: fallible on ZGC, collect-and-retry, and on a
                // refusal FALL THROUGH to the native funnel below, which
                // throws the OutOfMemoryError (it used to abort the process).
                if let Some(object) = jit_box_cold_alloc(vm, &mut *thread, class_id) {
                    // Descriptor-typed write (`Integer.value`, field 0, `I`) —
                    // this cold arm's allocator may pick a non-legacy layout,
                    // so keep the layout-aware store.
                    vm.mem.heap.set_field_as(object, 0, Value::Int(value), b'I');
                    // Object-return handoff root (see `call_integer_native_raw`).
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
            }
        }
    }
    // In-range fast path (round 9 wave 10, collect10; landed by wave 10b,
    // wire10b): a value this VM's box cache already holds is answered from it,
    // without the `safe_native_call` funnel -- the same object
    // `native_integer_value_of` would return (it reads the same table under
    // the same lock). A cold slot, an unresolved bound or a value past it
    // answers `None` and falls through to the native, which fills the slot and
    // discovers the wrapper class for the arm above. The cached object is
    // rooted by the cache (`gc_scan_value_of_cache_roots`), which is why the
    // native path below returns it without a pending-return handoff too.
    // MEASURED (w9b, `BoxProbe put`): `safe_native_call_impl` 18.8 % +
    // `native_integer_value_of` 17.7 % self is this path.
    if !beyond_cache {
        if let Some(object) =
            cratonvm_native_builtins::lang_math::integer_cache_peek(vm.vm_identity, value)
        {
            return object.as_ptr() as i64;
        }
    }
    // Cold / in-range / redefine-window path: canonical native callback via
    // the full safe-native-call wrapper (identity cache; also discovers the
    // real wrapper ClassId for the fast path above).
    let mut _jit_borrow = None;
    let held: Option<&mut JvmThread> = match given.as_deref_mut() {
        Some(t) => Some(t),
        None => jit_thread_mut().map(|(t, g)| {
            _jit_borrow = Some(g);
            t
        }),
    };
    let Some(thread) = held else {
        // No JIT thread context — cannot safely run the native. Signal the
        // deopt sentinel; the caller's post-invoke check bails to the
        // interpreter, which re-dispatches through the normal path.
        //
        // r10 `help`: that last clause was FALSE until the flag below was
        // added. `jit_bridge`'s post-JIT path takes its `result == i64::MIN`
        // arm only `&& deopt_signaled`; with every signal clear it pushes the
        // sentinel as a real value, and this helper returns a REFERENCE, so
        // `i64::MIN` would land in an oop slot.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    let arg = Value::Int(value);
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_value_of,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return handle_jit_dispatch_error(vm, thread, error, &INTEGER_VALUE_OF_INFO),
    };
    match result {
        Some(Value::Object(Some(object))) => {
            INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm.vm_identity,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
            object.as_ptr() as i64
        }
        _ => 0,
    }
}

/// Synthetic call-site info for [`jit_integer_int_value_direct`]'s
/// generic-dispatch fallback (invalid non-null receiver — a shape the
/// verifier rules out for a `final`-class receiver, kept for defensive
/// parity with `call_integer_native_raw`'s bail-to-dispatch behavior).
static INTEGER_INT_VALUE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Integer",
    method_name: "intValue",
    descriptor: "()I",
    num_jit_args: 1,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `invokevirtual Integer.intValue()` sites
/// whose constant-pool class is exactly `java/lang/Integer` (a `final`
/// class, so the site is statically monomorphic — no receiver guard
/// needed; only `null` remains, which throws NPE per JVMS).
///
/// Mirrors the `IntegerNativeKind::IntValue` arm of
/// [`call_integer_native_raw`]: a heap-validated field-0 read that cannot
/// allocate or safepoint. Null receiver → pending-NPE + deopt sentinel
/// (the canonical implicit-NPE signal). A non-null receiver that fails
/// heap validation falls back to the full generic dispatcher, exactly like
/// the existing arm's `None` return.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_integer_int_value_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_integer_int_value_direct_body(vm_ptr, receiver);
    contain(
        "jit_integer_int_value_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_integer_int_value_direct`], run under its panic guard.
unsafe fn jit_integer_int_value_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let census = crate::runtime::interp_census::direct_binds_enabled();
    let raw = receiver as u64;
    if raw == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        // The arena-membership probe stays. Nothing on this path has read the
        // receiver's header yet, so this probe is the ONLY thing standing
        // between a stale/fabricated argument and the `get_field` dereference
        // below; the alignment and canonical-address tests above reject wild
        // bit patterns but not a plausible-looking non-object address. It also
        // costs ~0 here (see the hashmap-half-gap closeout doc: restoring all
        // three probes measured inside run-to-run noise).
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            if census {
                crate::runtime::interp_census::note_int_value_direct(false);
            }
            return match vm.mem.heap.get_field(object, 0) {
                Value::Int(value) => value as i64,
                _ => 0,
            };
        }
    }
    // Defensive fallback: hand the call to the generic dispatcher (same
    // machinery the non-direct site would have used).
    if census {
        crate::runtime::interp_census::note_int_value_direct(true);
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &INTEGER_INT_VALUE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site info for [`jit_long_value_of_direct`]'s error path —
/// same role as [`INTEGER_VALUE_OF_INFO`], and read only for diagnostics and
/// exception context.
static LONG_VALUE_OF_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Long",
    method_name: "valueOf",
    descriptor: "(J)Ljava/lang/Long;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Synthetic call-site info for [`jit_long_long_value_direct`]'s
/// generic-dispatch fallback (a non-null receiver that fails heap validation —
/// defensive parity with [`INTEGER_INT_VALUE_INFO`]).
static LONG_LONG_VALUE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Long",
    method_name: "longValue",
    descriptor: "()J",
    num_jit_args: 1,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

thread_local! {
    /// Real `java/lang/Long` class discovered from the first ordinary
    /// `valueOf` result in each VM, as `(vm_identity, class id)` — the
    /// `Long` twin of [`INTEGER_WRAPPER_CLASS_CACHE`], and separate from it
    /// because the two wrappers are different classes with different layouts.
    static LONG_WRAPPER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Thin direct-call target for JIT `invokestatic Long.valueOf(J)` sites
/// (registered into `cratonvm_jit::LONG_VALUE_OF_DIRECT_FN` by
/// `build_helpers`; the recognition lives in `jit::try_compile`).
///
/// Line-for-line the same contract as [`jit_integer_value_of_direct`], against
/// the same registered native's semantics
/// (`lang_math.rs::native_long_value_of`, reached here through
/// `intrinsics::long::intrinsic_long_value_of`, which is a verbatim delegation
/// to it):
///
///  * out-of-cache values allocate a fresh wrapper through the mutator's normal
///    TLAB path and publish it via `native_pending_return` (the established
///    JIT→native object-return handoff root);
///  * `-128..=127`, the cold pre-discovery case, and any class-redefine window
///    route through `safe_native_call` to the canonical native callback, so the
///    JLS `Long`-cache identity contract is that native's, not a copy of it;
///  * errors (OOM) route through `handle_jit_dispatch_error` exactly like the
///    dispatch helper.
///
/// The argument arrives as a full 64-bit word — unlike the `Integer` twin
/// there is no `as i32` narrowing step, and there must not be one: the JIT
/// passes a category-2 value in one operand slot.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_long_value_of_direct(vm_ptr: i64, value: i64) -> i64 {
    let f = || jit_long_value_of_direct_body(vm_ptr, value);
    let bits = contain(
        "jit_long_value_of_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_long_value_of_direct`], run under its panic guard.
unsafe fn jit_long_value_of_direct_body(vm_ptr: i64, value: i64) -> i64 {
    jit_long_value_of_direct_body_on(vm_ptr, value, None)
}

/// [`jit_long_value_of_direct_body`] on a thread the caller already holds
/// (JIT round 23 wave 3b, lane L2; see [`jit_integer_value_of_direct_body_on`]).
unsafe fn jit_long_value_of_direct_body_on(
    vm_ptr: i64,
    value: i64,
    mut given: Option<&mut JvmThread>,
) -> i64 {
    // Same Rust<->JIT boundary bookkeeping as `jit_invoke_dispatch`.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_box_helper_entry_flush(vm_ptr, given.is_some());
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if !(-128..=127).contains(&value) {
        let vm_key = vm.vm_identity;
        let cached_class = LONG_WRAPPER_CLASS_CACHE.with(|cache| {
            cache
                .get()
                .filter(|(cached_vm, _)| *cached_vm == vm_key)
                .map(|(_, raw)| ClassId::new(raw))
                .filter(|class_id| !class_was_redefined(vm, *class_id))
        });
        if let Some(class_id) = cached_class {
            // Wave 3b: the caller's thread when it holds one (see `_on`).
            let mut _jit_borrow = None;
            let held: Option<&mut JvmThread> = match given.as_deref_mut() {
                Some(t) => Some(t),
                None => jit_thread_mut().map(|(t, g)| {
                    _jit_borrow = Some(g);
                    t
                }),
            };
            if let Some(thread) = held {
                thread_local! {
                    // (vm_key, class_id, slots). The exact-class redefine gate
                    // above prevents use after that class's layout changes.
                    static LONG_ALLOC_SLOTS: std::cell::Cell<Option<(usize, u32, u32)>> =
                        const { std::cell::Cell::new(None) };
                }
                let slots = LONG_ALLOC_SLOTS.with(|cache| {
                    if let Some((vk, cid, slots)) = cache.get() {
                        if vk == vm_key && cid == class_id.as_u32() {
                            return slots as usize;
                        }
                    }
                    let resolved = vm
                        .classes
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.num_total_fields.max(1))
                        .unwrap_or(1);
                    // Cast: field counts are far below u32::MAX.
                    cache.set(Some((vm_key, class_id.as_u32(), resolved as u32)));
                    resolved
                });
                // The shape planner, not a bare legacy size: every TLAB object
                // site has to agree with the header its allocation will be
                // stamped with. See `plan_tlab_object_shape`.
                //
                // Memoized per `(vm_key, class_id, slots)` exactly as the
                // `Integer` twin's `INTEGER_ALLOC_PLAN` is, and under the same
                // two conditions: the `class_was_redefined` screen above, and
                // no memo while `tlab_shape_planner_is_instrumented` (round 11:
                // this twin re-planned on every escaping box).
                thread_local! {
                    // (vm_key, class_id, slots, requested_size)
                    static LONG_ALLOC_PLAN: std::cell::Cell<
                        Option<(usize, u32, u32, u32)>,
                    > = const { std::cell::Cell::new(None) };
                }
                let planner_instrumented =
                    crate::runtime::interpreter::tlab_shape_planner_is_instrumented();
                let memo_key = (vm_key, class_id.as_u32(), slots as u32);
                let cached_plan = (!planner_instrumented)
                    .then(|| LONG_ALLOC_PLAN.with(|c| c.get()))
                    .flatten()
                    .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                    .map(|(_, _, _, size)| size as usize);
                let requested_size = match cached_plan {
                    Some(size) => size,
                    None => {
                        let (size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
                            vm,
                            class_id,
                            slots,
                            crate::runtime::interpreter::tlab_site::JIT_HELPER,
                        );
                        if !planner_instrumented {
                            if let Ok(size_u32) = u32::try_from(size) {
                                LONG_ALLOC_PLAN.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, size_u32)))
                                });
                            }
                        }
                        size
                    }
                };
                let tlab_object = if requested_size <= cratonvm_gc::tlab::tlab_max_alloc() {
                    crate::runtime::interpreter::tlab_alloc_object(
                        thread,
                        vm,
                        class_id,
                        slots,
                        requested_size,
                    )
                } else {
                    None
                };
                if let Some(object) = tlab_object {
                    // Raw primitive-cell write, exactly as the `Integer` twin
                    // does and for the same reason: this arm JUST allocated
                    // `object` through the legacy TLAB path (`init_object_header`,
                    // zeroed 16-byte `Value` cells), so field 0 is the `Value`
                    // cell at `HEADER_SIZE`. A primitive store takes no write
                    // barrier.
                    // ...but ONLY when it really is legacy -- see the
                    // `Integer` twin for the miscompile that premise produced.
                    // SAFETY: `object` was just allocated here, so its first
                    // HEADER_SIZE bytes are a live, fully written `ObjectHeader`.
                    let header =
                        unsafe { &*(object.as_ptr() as *const cratonvm_types::ObjectHeader) };
                    let compact = cratonvm_types::is_compact_object(header);
                    if compact {
                        // The packed `value` cell, memoized like the `Integer`
                        // twin's `INTEGER_VALUE_CELL` and for the same reason
                        // (`set_field_as` is three layout lookups for one
                        // store). Restricted to an 8-byte `Long` cell at an
                        // 8-aligned address; any other shape declines to
                        // `set_field_as`.
                        thread_local! {
                            // (vm_key, class_id, slots, offset)
                            static LONG_VALUE_CELL: std::cell::Cell<
                                Option<(usize, u32, u32, u32)>,
                            > = const { std::cell::Cell::new(None) };
                        }
                        let offset = LONG_VALUE_CELL
                            .with(|c| c.get())
                            .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                            .map(|(_, _, _, offset)| offset as usize)
                            .or_else(|| {
                                // An ABSOLUTE displacement (header included)
                                // since the 8-byte compact header.
                                let (offset, storage) =
                                    cratonvm_types::compact_object_field_disp(header, 0)?;
                                if storage != cratonvm_types::FieldStorageKind::Long
                                    || offset % 8 != 0
                                {
                                    return None;
                                }
                                let offset_u32 = u32::try_from(offset).ok()?;
                                LONG_VALUE_CELL.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, offset_u32)))
                                });
                                Some(offset)
                            });
                        match offset {
                            // SAFETY: `object` is this arm's own just-allocated
                            // compact body (8-aligned), and `offset` is field
                            // 0's byte offset in the layout this object's
                            // header names, checked to be an 8-aligned `Long`
                            // cell. A primitive store into a fresh object takes
                            // no write barrier.
                            Some(offset) => unsafe {
                                cratonvm_types::write_compact_field(
                                    object.as_ptr().add(offset),
                                    cratonvm_types::FieldStorageKind::Long,
                                    Value::Long(value),
                                    std::sync::atomic::Ordering::Relaxed,
                                )
                            },
                            None => vm
                                .mem
                                .heap
                                .set_field_as(object, 0, Value::Long(value), b'J'),
                        }
                    } else {
                        // SAFETY: `object` is a live legacy-layout allocation
                        // with >= 1 slot (`slots.max(1)` above) -- now CHECKED
                        // immediately above, not assumed; the cell is
                        // exclusively ours until published below.
                        unsafe {
                            std::ptr::write(
                                object.as_ptr().add(cratonvm_gc::heap::HEADER_SIZE) as *mut Value,
                                Value::Long(value),
                            );
                        }
                    }
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                // zr z3/lt: see the `Integer.valueOf` twin -- a refusal falls
                // through to the native funnel below, which throws.
                if let Some(object) = jit_box_cold_alloc(vm, &mut *thread, class_id) {
                    // Descriptor-typed write (`Long.value`, field 0, `J`) —
                    // this cold arm's allocator may pick a non-legacy layout,
                    // so keep the layout-aware store.
                    vm.mem
                        .heap
                        .set_field_as(object, 0, Value::Long(value), b'J');
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
            }
        }
    }
    // In-cache-range fast path (round 9 wave 10b, wire10b; the `Integer`
    // twin's reasoning): a filled `LongCache` slot is answered directly, the
    // same object `native_long_value_of` would return. A cold slot falls
    // through to the native, which fills it.
    if (-128..=127).contains(&value) {
        if let Some(object) =
            cratonvm_native_builtins::lang_math::long_cache_peek(vm.vm_identity, value)
        {
            return object.as_ptr() as i64;
        }
    }
    // Cold / in-cache-range / redefine-window path: canonical native callback
    // via the full safe-native-call wrapper (identity cache; also discovers the
    // real wrapper ClassId for the fast path above).
    let mut _jit_borrow = None;
    let held: Option<&mut JvmThread> = match given.as_deref_mut() {
        Some(t) => Some(t),
        None => jit_thread_mut().map(|(t, g)| {
            _jit_borrow = Some(g);
            t
        }),
    };
    let Some(thread) = held else {
        // No JIT thread context — signal the deopt sentinel; the caller's
        // post-invoke check bails to the interpreter, which re-dispatches.
        // The flag is what makes that true — see the `Integer.valueOf` twin.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    let arg = Value::Long(value);
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        cratonvm_native_builtins::intrinsics::long::intrinsic_long_value_of,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return handle_jit_dispatch_error(vm, thread, error, &LONG_VALUE_OF_INFO),
    };
    match result {
        Some(Value::Object(Some(object))) => {
            LONG_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm.vm_identity,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
            object.as_ptr() as i64
        }
        _ => 0,
    }
}

/// Thin direct-call target for JIT `invokevirtual Long.longValue()` sites whose
/// constant-pool class is exactly `java/lang/Long` (a `final` class, so the site
/// is statically monomorphic — no receiver guard needed; only `null` remains,
/// which throws NPE per JVMS).
///
/// Mirrors [`jit_integer_int_value_direct`] and the registered
/// `lang_math.rs::native_wrapper_long_value` it stands in for, INCLUDING that
/// native's `Value::Int` arm: reflection can legally leave an int-tagged raw
/// slot in a `Long` wrapper, and a `()J` caller must see the widened value
/// rather than the compact tag bits. Dropping that arm here would make the fast
/// path disagree with the funnel on exactly the receiver the funnel was written
/// for.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_long_long_value_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_long_long_value_direct_body(vm_ptr, receiver);
    contain(
        "jit_long_long_value_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_long_long_value_direct`], run under its panic guard.
unsafe fn jit_long_long_value_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let census = crate::runtime::interp_census::direct_binds_enabled();
    let raw = receiver as u64;
    if raw == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        // The arena-membership probe stays, for the reason the `Integer` twin
        // states: nothing on this path has read the receiver's header yet, so
        // it is the only thing between a fabricated argument and the
        // `get_field` dereference.
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            match vm.mem.heap.get_field(object, 0) {
                Value::Long(value) => {
                    if census {
                        crate::runtime::interp_census::note_long_value_direct(false);
                    }
                    return value;
                }
                Value::Int(value) => {
                    if census {
                        crate::runtime::interp_census::note_long_value_direct(false);
                    }
                    return i64::from(value);
                }
                // Anything else is a shape the registered native answers 0 for;
                // hand it to the generic dispatcher rather than guessing, so the
                // two paths cannot disagree.
                _ => {}
            }
        }
    }
    // Defensive fallback: hand the call to the generic dispatcher (same
    // machinery the non-direct site would have used).
    if census {
        crate::runtime::interp_census::note_long_value_direct(true);
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &LONG_LONG_VALUE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// A one-entry, per-thread memo of "the raw address `recv` holds a live object
/// of class `class_id`", valid only inside one GC epoch.
///
/// # Why
///
/// `ZgcRealHeap::is_object_address` plus `ZObjectStarts::contains` measured
/// **16 %** of `JdkZlibIntegrationTest#testHugeDecompress` once the funnel was
/// off its three hot calls -- and the receiver it re-validates is the SAME
/// object 268 million times running: one `MessageDigest`, one pooled
/// `ByteBuffer` per megabyte chunk.
///
/// # Why the epoch is the whole safety argument
///
/// The hazard a memo has to answer is an address whose object died and whose
/// storage was handed to something else. That cannot happen inside one epoch:
/// an address occupied by a live object at memo time is not on any free list
/// until a collection reclaims it, and every collection bumps
/// `VmHeap::collection_count`. A memo from a previous epoch therefore never
/// matches, and a moving collection is covered twice over -- the compiled
/// frame's receiver is rewritten to the new address, which misses on `recv`
/// as well.
///
/// Per-thread, so no lock and no cross-thread visibility question. A miss costs
/// exactly what every call used to pay.
#[derive(Clone, Copy)]
struct DirectReceiverMemo {
    /// The VM whose heap `recv` was proven in (`SharedVm::vm_identity`).
    /// gc-common w36-a: the memo is per THREAD, and one thread can run two
    /// VMs in turn (the `vm` test binary under `--test-threads=1` runs every
    /// test on its main thread; an embedder may too). A disposed VM's heap
    /// range can be mapped again by the next VM, whose collection count
    /// starts from the same value, so `(recv, epoch)` alone could serve the
    /// old VM's class id and identity hash for the new VM's object -- a
    /// wrong-class VarHandle plan admission or a digest fed another's byte.
    /// Identities are never recycled, so `(vm, recv, epoch)` cannot collide.
    vm: usize,
    recv: usize,
    epoch: u64,
    class_id: u32,
    /// The `NativeContext`-compatible identity hash, so a helper keyed by it
    /// (`MessageDigest.update`) does not re-derive one per call. Never `0` for
    /// a filled entry: `java_identity_hash` refuses to hand back zero.
    identity: i32,
}

/// FOUR entries, and the number is measured rather than chosen.
///
/// A ONE-entry memo removed nothing on the workload it was written for:
/// `testHugeDecompress`'s inner loop alternates `in.writeByte(b)` with
/// `digest.update(b)`, so consecutive calls arrive with two different receivers
/// and a single slot thrashes on every one. `is_object_address` +
/// `ZObjectStarts::contains` stayed at 15 % of the profile across the change.
/// EIGHT, for the same reason four beat one. `testHugeDecompress`'s inner loop
/// touches four distinct receivers per byte once the `VarHandle` read is on the
/// memo too — the digest, the pooled `ByteBuffer`, the `RefCnt` the accessibility
/// check reads, and the `VarHandle` itself — and the loop's other calls bring
/// their own. Eight leaves headroom for that without the scan mattering: it is
/// eight predictable compares against a probe that walks a sorted page index.
const DIRECT_RECEIVER_MEMO_WAYS: usize = 8;

thread_local! {
    static DIRECT_RECEIVER_MEMO: [std::cell::Cell<DirectReceiverMemo>; DIRECT_RECEIVER_MEMO_WAYS] =
        const {
            [const { std::cell::Cell::new(DirectReceiverMemo {
                vm: 0, recv: 0, epoch: 0, class_id: 0, identity: 0,
            }) }; DIRECT_RECEIVER_MEMO_WAYS]
        };
    /// Round-robin victim for the memo above.
    static DIRECT_RECEIVER_MEMO_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `is_object_address` + `class_id_of` (+ the identity hash) for a thin direct
/// helper's receiver, memoised per GC epoch. `None` means the address is not a
/// live object base and the caller must decline.
///
/// # Safety
/// `raw` is a receiver argument from JIT-compiled code; `vm` is live.
#[inline]
unsafe fn direct_receiver_facts(vm: &SharedVm, raw: usize) -> Option<(u32, i32)> {
    let epoch = vm.mem.address_epoch();
    let vm_id = vm.vm_identity;
    // Read individual entries in place: copying the whole eight-way table on
    // every byte access made the memo itself a measured hot path.
    if let Some(facts) = DIRECT_RECEIVER_MEMO.with(|ways| {
        ways.iter().find_map(|cell| {
            let way = cell.get();
            (way.recv == raw && way.epoch == epoch && way.vm == vm_id && way.class_id != 0)
                .then_some((way.class_id, way.identity))
        })
    }) {
        return Some(facts);
    }
    let object = vm.mem.heap.is_object_address(raw)?;
    // Round 11 wave 13 (lane `icwide`): 0 for an array, whose header carries
    // its COMPONENT's id (`jit_dispatch_class`). Every caller reads this id
    // as the receiver's CLASS and declines on 0: a `VarHandle`'s receiver
    // coordinate is `Object`-typed at the call site, so an `H[]` reached
    // `varhandle_plan_admits_receiver` as an `H` and had a field of `H`
    // read from, written into or CASed in the array's element storage.
    let class_id = jit_dispatch_class_validated(vm, object)
        .class_id()
        .map_or(0, |cid| cid.as_u32());
    // The SAME key `NativeContextImpl::identity_hash_code` computes: the heap
    // answer, then the monitor's displaced-hash arbitration. A locked object's
    // mark word cannot hold a hash, so the heap answer alone is the wrong key
    // for any side table the natives maintain.
    let heap = &vm.mem.heap;
    let identity =
        vm.threads
            .monitors
            .java_identity_hash(object, heap.identity_hash_code(object), || {
                heap.next_identity_hash()
            });
    if class_id != 0 {
        let slot = DIRECT_RECEIVER_MEMO_NEXT.with(|c| {
            let i = c.get();
            c.set((i + 1) % DIRECT_RECEIVER_MEMO_WAYS);
            i
        });
        DIRECT_RECEIVER_MEMO.with(|ways| {
            ways[slot].set(DirectReceiverMemo {
                vm: vm_id,
                recv: raw,
                epoch,
                class_id,
                identity,
            })
        });
    }
    Some((class_id, identity))
}

/// Synthetic call-site info for the `ByteBuffer` single-byte element helpers'
/// DECLINE edge. `num_jit_args` counts the receiver, the way an instance
/// site's does.
static NIO_BYTEBUFFER_PUT_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/nio/ByteBuffer",
    method_name: "put",
    descriptor: "(IB)Ljava/nio/ByteBuffer;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static NIO_BYTEBUFFER_GET_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/nio/ByteBuffer",
    method_name: "get",
    descriptor: "(I)B",
    num_jit_args: 2,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The shared prologue of both `ByteBuffer` single-byte helpers: prove the
/// receiver is a direct buffer of a class the FUNNEL has already served with
/// the modelled layout, and return the element's absolute address.
///
/// It reads the same three fields `native-io`'s `dbb_elem_addr` reads, through
/// the same descriptor-aware accessor, in the same order, and refuses on the
/// same conditions — a negative or out-of-limit index, a read-only carrier for
/// a write, a non-positive address. That is deliberate duplication of a
/// SEQUENCE, not of a LAYOUT: the three field indices come from
/// `elem_fastpath::slots()`, which is what the native itself resolved, so
/// there is no second copy of the layout to drift.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
unsafe fn dbb_direct_elem_addr(
    vm: &SharedVm,
    receiver: i64,
    index: i64,
    for_write: bool,
) -> Option<i64> {
    let raw = receiver as u64;
    if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let (address_slot, limit_slot, read_only_slot) =
        cratonvm_native_io::direct_buffer::elem_fastpath::slots()?;
    // The arena-membership probe, for the reason the `Long.longValue` twin
    // states: nothing on this path has read the receiver's header yet, so it is
    // the only thing between a fabricated argument and the field reads below.
    // Memoised per GC epoch -- see `direct_receiver_facts`.
    let (class_id, _identity) = direct_receiver_facts(vm, raw as usize)?;
    if !cratonvm_native_io::direct_buffer::elem_fastpath::class_is_served(class_id, for_write) {
        return None;
    }
    // The memo proved this address is a live object base in this epoch, so the
    // reference is reconstructible without a second probe -- the same step
    // `jit_hashmap_get_direct` takes after its own exact-receiver screen.
    let object = ObjectRef::from_raw(raw as usize as *mut u8);
    // W7-89 follow-up: `class_is_served` is keyed by CLASS, and
    // `java/nio/DirectByteBuffer` is the one class both a plain
    // `allocateDirect()` buffer and an FFM `MemorySegment.asByteBuffer()` view
    // share -- so a class the funnel marked served from the former must still
    // be refused PER RECEIVER when a later call at this same compiled site
    // sees the latter. This helper has no way to check the segment's session
    // liveness/confinement itself (this crate is below `native-builtins`,
    // where that check lives), so it declines outright and falls through to
    // the generic dispatcher, which runs the real `DirectByteBuffer` bytecode
    // -- `session()` -> `ScopedMemoryAccess.getByte` -> `checkValidState()` --
    // exactly as the interpreter's own bail (`dbb_has_ffm_segment`) does.
    if let Some(segment_slot) = cratonvm_native_io::direct_buffer::elem_fastpath::segment_slot() {
        if matches!(
            vm.mem.heap.get_field_as(object, segment_slot, b'L'),
            cratonvm_types::Value::Object(Some(_))
        ) {
            return None;
        }
    }
    // H12-1, as on the `HashMap` helpers. The gate is `for_write`'s own
    // choice, because that flag is exactly the choice between `ByteBuffer.put`
    // and `ByteBuffer.get` — two registered rows, two policy questions, and
    // the decline edges below dispatch through two different `JitInvokeInfo`s.
    // Placed after the receiver screens so the counters count calls the fast
    // path would otherwise have SERVED.
    let gate: &'static DirectHelperGate = if for_write {
        &NIO_BYTEBUFFER_PUT_BYTE_GATE
    } else {
        &NIO_BYTEBUFFER_GET_BYTE_GATE
    };
    if jit_direct_helper_refused(vm, gate) {
        return None;
    }
    let cratonvm_types::Value::Int(limit) = vm.mem.heap.get_field_as(object, limit_slot, b'I')
    else {
        return None;
    };
    // Cast: the JIT operand stack carries one sign-extended i64 per int.
    let idx = index as i32;
    if idx < 0 || idx >= limit {
        return None;
    }
    if for_write
        && !matches!(
            vm.mem.heap.get_field_as(object, read_only_slot, b'Z'),
            cratonvm_types::Value::Int(0)
        )
    {
        return None;
    }
    let cratonvm_types::Value::Long(address) = vm.mem.heap.get_field_as(object, address_slot, b'J')
    else {
        return None;
    };
    if address <= 0 {
        return None;
    }
    address.checked_add(i64::from(idx))
}

/// Thin direct-call target for JIT `ByteBuffer.put(int,byte)` sites.
///
/// See `cratonvm_jit::NIO_BYTEBUFFER_PUT_BYTE_DIRECT_FN` for the census. The
/// fast path writes one byte and returns the receiver, which is what
/// `put(int,byte)` returns; **every** case it is not certain of — a null or
/// fabricated receiver, a class the funnel has not served, an out-of-limit
/// index, a read-only carrier, an address the off-heap store refuses — falls
/// through to the generic dispatcher, so the exception a caller sees is still
/// the registered native's.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_dbb_put_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    index: i64,
    value: i64,
) -> i64 {
    let f = || jit_dbb_put_byte_direct_body(vm_ptr, receiver, index, value);
    contain(
        "jit_dbb_put_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_dbb_put_byte_direct`], run under its panic guard.
unsafe fn jit_dbb_put_byte_direct_body(vm_ptr: i64, receiver: i64, index: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(at) = dbb_direct_elem_addr(vm, receiver, index, true) {
        // Cast: only the low 8 bits are the Java `byte`; the operand stack
        // widened it to int.
        let byte = value as u8;
        // Classified by the TAG BIT and not by an arena liveness probe, for the
        // reason `NativeContextImpl::copy_to_native_memory` states: a tagged
        // handle whose arena was freed must be DECLINED, never copied as an OS
        // pointer.
        let stored = if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(at) {
            cratonvm_native_builtins::unsafe_arena_copy_in(at, &[byte])
        } else if at > 0 {
            // SAFETY: `at` is `address + index` for an index the carrier's own
            // `limit` admits, and `address` is the block this buffer owns.
            std::ptr::write(at as *mut u8, byte);
            true
        } else {
            false
        };
        if stored {
            cratonvm_jit::NIO_BYTE_ELEMENT_SERVED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return receiver;
        }
    }
    cratonvm_jit::NIO_BYTE_ELEMENT_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, index, value];
    jit_invoke_dispatch(
        vm_ptr,
        &NIO_BYTEBUFFER_PUT_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// Thin direct-call target for JIT `ByteBuffer.get(int)` sites — the read twin
/// of [`jit_dbb_put_byte_direct`], with the same decline contract.
///
/// The value is returned SIGN-EXTENDED through `i8`, which is what
/// `dbb_get_abs` produces (`Value::Int(i32::from(byte as i8))`) and what every
/// `b < 0` caller expects.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_dbb_get_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    index: i64,
) -> i64 {
    let f = || jit_dbb_get_byte_direct_body(vm_ptr, receiver, index);
    contain(
        "jit_dbb_get_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_dbb_get_byte_direct`], run under its panic guard.
unsafe fn jit_dbb_get_byte_direct_body(vm_ptr: i64, receiver: i64, index: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(at) = dbb_direct_elem_addr(vm, receiver, index, false) {
        let mut byte = [0u8; 1];
        let loaded = if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(at) {
            cratonvm_native_builtins::unsafe_arena_copy_out(at, &mut byte)
        } else if at > 0 {
            // SAFETY: as the store twin — inside the carrier's own block.
            byte[0] = std::ptr::read(at as *const u8);
            true
        } else {
            false
        };
        if loaded {
            cratonvm_jit::NIO_BYTE_ELEMENT_SERVED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return i64::from(byte[0] as i8);
        }
    }
    cratonvm_jit::NIO_BYTE_ELEMENT_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, index];
    jit_invoke_dispatch(
        vm_ptr,
        &NIO_BYTEBUFFER_GET_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Synthetic call-site info for [`jit_md_update_byte_direct`]'s decline edge.
static MD_UPDATE_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/security/MessageDigest",
    method_name: "update",
    descriptor: "(B)V",
    num_jit_args: 2,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `MessageDigest.update(byte)` sites.
///
/// See `cratonvm_jit::MD_UPDATE_BYTE_DIRECT_FN` for the census. The fast path
/// appends one byte to the accumulator the registered native keeps for this
/// receiver, and it may do so only when BOTH of the native's own conditions
/// hold: the receiver's class is the exact one `md_receiver_is_ours` accepted,
/// and the side table already has an entry for it.
///
/// The key is computed the way `NativeContextImpl::identity_hash_code`
/// computes it — heap answer, then `Monitors::java_identity_hash` — and NOT
/// from the heap alone. A locked object's mark word cannot hold a hash, so the
/// heap answer for one is `0` and the monitor holds the displaced value; using
/// the heap answer directly would file a byte under a key the native never
/// uses. Getting it wrong would MISS rather than corrupt (the entry is looked
/// up, never created), but a miss on every call is a fast path that does
/// nothing while looking installed.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_md_update_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    value: i64,
) -> i64 {
    let f = || jit_md_update_byte_direct_body(vm_ptr, receiver, value);
    contain(
        "jit_md_update_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_md_update_byte_direct`], run under its panic guard.
unsafe fn jit_md_update_byte_direct_body(vm_ptr: i64, receiver: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    'fast: {
        let raw = receiver as u64;
        if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            break 'fast;
        }
        // The arena-membership probe: the only thing between a fabricated
        // argument and the class-header read below. Memoised per GC epoch --
        // see `direct_receiver_facts`. This receiver is the SAME digest for
        // every one of the 268 million bytes a side, so the memo hits on all
        // but the first (and the entry carries the identity key, which is what
        // this helper would otherwise re-derive per byte).
        let Some((class_id, key)) = direct_receiver_facts(vm, raw as usize) else {
            break 'fast;
        };
        if !md_update_class_is_ours(vm, class_id) {
            break 'fast;
        }
        if jit_direct_helper_refused(vm, &MD_UPDATE_BYTE_GATE) {
            break 'fast;
        }
        // Cast: only the low 8 bits are the Java `byte`; the operand stack
        // widened it to int. The VM names the row too (gc-common w36-a): a
        // lone row of another VM's digest is never this receiver's.
        if !cratonvm_native_builtins::jca::message_digest::update_fastpath::append_byte(
            vm.vm_identity,
            key,
            value as u8,
        ) {
            break 'fast;
        }
        cratonvm_jit::MD_UPDATE_BYTE_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::MD_UPDATE_BYTE_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, value];
    jit_invoke_dispatch(
        vm_ptr,
        &MD_UPDATE_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

thread_local! {
    /// The last `(vm_identity, class id, verdict)` [`md_update_class_is_ours`]
    /// resolved on this thread. One entry: a hashing loop feeds one digest.
    static MD_OURS_CLASS_MEMO: std::cell::Cell<(usize, u32, bool)> =
        const { std::cell::Cell::new((0, 0, false)) };
}

/// Is `class_id`, in `vm`, the exact class the `MessageDigest` accumulator
/// natives own (`update_fastpath::class_name_is_ours`)?
///
/// gc-common w36-a (`common-w22b-md-update-fastpath-class-id-is-process-wide`):
/// this used to read one process-wide id latched by the first VM whose native
/// accepted a digest. `ClassId`s are minted per VM, so in a second VM that id
/// could name a provider's digest subclass, whose bytes were then appended to
/// another digest's accumulator. Now the class is named by THIS VM's own class
/// table, memoised per thread on `(vm_identity, class id)`. Identities are
/// never recycled, so a line never answers for another VM; a `true` line names
/// the bootstrap `java/security/MessageDigest`, which is never unloaded; and a
/// `false` line that went stale (were an id ever reused) could only decline
/// to the native. Steady state: one thread-local compare; a miss takes the
/// class manager's read lock once.
fn md_update_class_is_ours(vm: &SharedVm, class_id: u32) -> bool {
    md_class_verdict_memoised(vm.vm_identity, class_id, || {
        vm.classes
            .class_manager
            .read()
            .get_class(ClassId::new(class_id))
            .map(|c| {
                cratonvm_native_builtins::jca::message_digest::update_fastpath::class_name_is_ours(
                    &c.name,
                )
            })
    })
}

/// The memo behind [`md_update_class_is_ours`]: `resolve` names the verdict
/// for `(vm_id, class_id)` on a miss (`None`: the class is unknown, which is
/// not "ours" and is not memoised).
fn md_class_verdict_memoised(
    vm_id: usize,
    class_id: u32,
    resolve: impl FnOnce() -> Option<bool>,
) -> bool {
    if class_id == 0 {
        return false;
    }
    let (memo_vm, memo_class, memo_ours) = MD_OURS_CLASS_MEMO.with(std::cell::Cell::get);
    if memo_vm == vm_id && memo_class == class_id {
        return memo_ours;
    }
    match resolve() {
        Some(ours) => {
            MD_OURS_CLASS_MEMO.with(|c| c.set((vm_id, class_id, ours)));
            ours
        }
        None => false,
    }
}

/// Synthetic call-site info for [`jit_preconditions_check_index_direct`]'s
/// cold arm — the out-of-range case, which must throw exactly what the
/// registered native throws.
static PRECONDITIONS_CHECK_INDEX_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/Preconditions",
    method_name: "checkIndex",
    descriptor: "(IILjava/util/function/BiFunction;)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `invokestatic
/// jdk/internal/util/Preconditions.checkIndex(int,int,BiFunction)` sites
/// (registered into `cratonvm_jit::PRECONDITIONS_CHECK_INDEX_DIRECT_FN` by
/// `build_helpers`, recognised in `jit::try_compile`'s ladder and the IR path).
///
/// **Why this one.** `--dump-native-registry`'s invocation census on
/// `probes/NioAccessorRate.java` put it at the TOP of the list — 4 000 000
/// calls for 800 000 `ByteBuffer` accessor operations, ~2.5 per accessor,
/// ahead of the store itself. `Objects.checkIndex` sits under every
/// `java.nio.Buffer` absolute accessor, every `String` index check and every
/// `List` bounds check, and it is a compare and a branch: paying the ~160 ns
/// generic native funnel for it is the single largest rung under
/// `HttpContentDecompressorTest.testZipBomb`
/// (httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md).
///
/// Fast path: `0 <= index < length` returns `index`, with no funnel, no
/// argument buffer, no `safe_native_call` wrapper. **Everything else — an
/// out-of-range index, a negative length, anything that must throw — falls
/// through to the generic dispatcher**, so the exception's class, message and
/// the `BiFunction` formatter selection stay byte-for-byte what the registered
/// native produces. That matters here more than usual: `throw_out_of_bounds`
/// reads `Preconditions`' three static formatters to tell
/// `StringIndexOutOfBoundsException` from `IndexOutOfBoundsException`, and
/// reimplementing that choice in the fast path is exactly how the wrong-class
/// bug the module comment warns about comes back.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_preconditions_check_index_direct(
    vm_ptr: i64,
    index: i64,
    length: i64,
    formatter: i64,
) -> i64 {
    let f = || jit_preconditions_check_index_direct_body(vm_ptr, index, length, formatter);
    contain(
        "jit_preconditions_check_index_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_preconditions_check_index_direct`], run under its panic guard.
unsafe fn jit_preconditions_check_index_direct_body(
    vm_ptr: i64,
    index: i64,
    length: i64,
    formatter: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // H12-1. `jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I`
    // is `bridge`, and the OSR door binds this helper with no policy question.
    // Declining takes the same generic dispatcher the throwing case already
    // uses, so the refusal costs a route, not a contract.
    //
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    let i = index as i32;
    let n = length as i32;
    if i >= 0 && n >= 0 && i < n && !jit_direct_helper_refused(vm, &PRECONDITIONS_CHECK_INDEX_GATE)
    {
        return i as i64;
    }
    // Throwing case (and any shape this fast path declines to judge): the
    // generic dispatcher runs the registered native, formatter and all.
    let args = [index, length, formatter];
    jit_invoke_dispatch(
        vm_ptr,
        &PRECONDITIONS_CHECK_INDEX_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`, opt-in): the LEAF TWIN of
/// [`jit_preconditions_check_index_direct`]. Its fast arm, verbatim, and `-1`
/// (an answer the helper never gives: it answers only an index in
/// `[0, length)`) where the helper would dispatch.
///
/// The contract the single-pass tail site relies on
/// (`x64/op_invoke.rs` `try_emit_sp_tail_leaf_twin`): this function cannot
/// reach the VM. It allocates nothing, raises nothing (a panic is RECORDED
/// and declines, never stashed as an `InternalError`), runs no Java and
/// dispatches nothing, so no stack walk can happen while it runs and the
/// caller owes it no safepoint publication. The gate read is the helper's
/// own per-call policy question (a memoised registry lookup and a per-thread
/// counter). A decline sends the caller to the helper itself, with its frame
/// published.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_preconditions_check_index_leaf(
    vm_ptr: i64,
    index: i64,
    length: i64,
    _formatter: i64,
) -> i64 {
    let f = || {
        // SAFETY: vm_ptr originates from JIT code compiled against this live
        // VM, exactly as for the helper this twins.
        let vm = &*(vm_ptr as *const SharedVm);
        let i = index as i32;
        let n = length as i32;
        if i >= 0
            && n >= 0
            && i < n
            && !jit_direct_helper_refused(vm, &PRECONDITIONS_CHECK_INDEX_GATE)
        {
            i64::from(i)
        } else {
            -1
        }
    };
    contain("jit_preconditions_check_index_leaf", OnPanic::RecordFor { vm_ptr }, -1, f)
}

/// Synthetic call-site infos for the cold arm of the two
/// [`jit_decimal_digits_latin1_int_direct`] / `_long_` helpers: the real
/// `DecimalDigits` bytecode, through the generic dispatcher.
static DECIMAL_DIGITS_LATIN1_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/DecimalDigits",
    method_name: "uncheckedGetCharsLatin1",
    descriptor: "(II[B)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};
static DECIMAL_DIGITS_LATIN1_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/DecimalDigits",
    method_name: "uncheckedGetCharsLatin1",
    descriptor: "(JI[B)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The digits `jdk.internal.util.DecimalDigits.uncheckedGetCharsLatin1`
/// writes for `value`, written the way it writes them: backwards from
/// `index` (exclusive), a `'-'` in front of a negative value, LATIN1 bytes.
/// Answers the new `charPos` (`index - digits`), or `None` -- writing
/// nothing -- when that range is not wholly inside `buf`, which the JDK
/// body would write past (it asserts the bounds only under `-esa`).
///
/// `i64` covers both overloads: an `int` argument arrives sign-extended,
/// and `unsigned_abs` makes `Integer.MIN_VALUE` / `Long.MIN_VALUE` exact.
fn decimal_digits_latin1_into(value: i64, index: i64, buf: &mut [u8]) -> Option<i64> {
    let mut scratch = [0u8; 20];
    let mut at = scratch.len();
    let mut magnitude = value.unsigned_abs();
    loop {
        at -= 1;
        scratch[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        at -= 1;
        scratch[at] = b'-';
    }
    let digits = &scratch[at..];
    let end = usize::try_from(index).ok()?;
    let start = end.checked_sub(digits.len())?;
    buf.get_mut(start..end)?.copy_from_slice(digits);
    // Cast: `start <= index`, which came in as an i64.
    Some(start as i64)
}

/// Thin direct-call targets for JIT `invokestatic
/// jdk/internal/util/DecimalDigits.uncheckedGetCharsLatin1(II[B)I` and
/// `(JI[B)I` sites (JIT round 16, lane strings, STR16-6;
/// `CRATONVM_JIT_DECIMAL_DIGITS_DIRECT`, opt-in).
///
/// **Why.** Every JDK 25 `int`/`long` -> decimal conversion --
/// `AbstractStringBuilder.append(int|long)`, `Integer.toString`,
/// `Long.toString`, `String.valueOf` -- ends here, and the JDK body writes
/// each digit pair through `uncheckedPutPairLatin1` ->
/// `uncheckedPutCharLatin1` -> `Unsafe.putByte`: two compiled-to-compiled
/// calls and one `Unsafe` helper call per DIGIT (the splicer refuses the
/// chain, `uncheckedPutCharLatin1` carries an `AssertionError` arm), so
/// `append(-i)` measured 3 124-3 480 ns against 235 for the call-site
/// intrinsic's path. This writes the same bytes in one call.
///
/// Fast path: `buf` a live `byte[]` and `[index - digits, index)` inside it.
/// Everything else -- a null or non-`byte[]` `buf`, a range the JDK body
/// would write out of bounds -- falls through to the generic dispatcher, so
/// the real bytecode decides what happens, exactly as it does today.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_decimal_digits_latin1_int_direct(
    vm_ptr: i64,
    value: i64,
    index: i64,
    buf: i64,
) -> i64 {
    let f = || {
        jit_decimal_digits_latin1_direct_body(
            vm_ptr,
            i64::from(value as i32),
            index,
            buf,
            &DECIMAL_DIGITS_LATIN1_INT_INFO,
        )
    };
    contain(
        "jit_decimal_digits_latin1_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// The `(JI[B)I` twin of [`jit_decimal_digits_latin1_int_direct`].
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_decimal_digits_latin1_long_direct(
    vm_ptr: i64,
    value: i64,
    index: i64,
    buf: i64,
) -> i64 {
    let f = || {
        jit_decimal_digits_latin1_direct_body(
            vm_ptr,
            value,
            index,
            buf,
            &DECIMAL_DIGITS_LATIN1_LONG_INFO,
        )
    };
    contain(
        "jit_decimal_digits_latin1_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of the two `jit_decimal_digits_latin1_*_direct` helpers, run under
/// their panic guards.
unsafe fn jit_decimal_digits_latin1_direct_body(
    vm_ptr: i64,
    value: i64,
    index: i64,
    buf: i64,
    info: &'static JitInvokeInfo,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // `index` is a Java `int`; the callers pass it sign-extended.
    let index = i64::from(index as i32);
    // j27 wave 3 (lane BACKLOG, STR16-P7): no per-call census here. The
    // served / declined counters were written on every call (one shared
    // cache line for every thread converting a number) and read by no one.
    if let Some(char_pos) = decimal_digits_latin1_serve(vm, value, index, buf) {
        return char_pos;
    }
    let args = [value, index, buf];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// The fast path of the `jit_decimal_digits_latin1_*_direct` helpers: the
/// same `byte[]` validation `raw_accessor_write_bytes` makes (aligned,
/// heap-resident, an array, a BYTE array -- `boolean[]` is not a `byte[]`
/// here) before [`decimal_digits_latin1_into`] writes into its payload.
unsafe fn decimal_digits_latin1_serve(
    vm: &SharedVm,
    value: i64,
    index: i64,
    buf: i64,
) -> Option<i64> {
    let raw = buf as u64;
    if buf == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array
        || vm.mem.heap.element_type_of_validated(object) != ArrayElementType::Byte
    {
        return None;
    }
    let length = vm.mem.heap.array_length(object);
    let base = vm.mem.heap.array_data_ptr(object)?;
    // SAFETY: `base` is the payload of a live `byte[]` of `length` elements,
    // and nothing between here and the write can move or free it (no
    // allocation, no safepoint).
    let payload = std::slice::from_raw_parts_mut(base, length);
    decimal_digits_latin1_into(value, index, payload)
}

/// Synthetic call-site info for the cold arms of
/// [`jit_byte_array_clone_direct`] and [`jit_object_clone_virtual_direct`]: a
/// VIRTUAL `clone()` through the generic dispatcher, which selects on the
/// receiver's runtime class (an override, or `Object.clone` for an array) and
/// raises the null receiver's NPE. j27 wave 3 (lane CLONE): one descriptor for
/// both (it was `[B`-only), named against `Object`, where `clone()` is
/// declared, so it reads true for every receiver it now stands for.
static CLONE_VIRTUAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "clone",
    descriptor: "()Ljava/lang/Object;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct helper for a compiled `byte[].clone()` (strings round 2,
/// `str17`; `CRATONVM_JIT_BYTE_ARRAY_CLONE_DIRECT`). An array receiver never
/// sits in a monomorphic inline cache, so every call took
/// `jit_invoke_virtual_mic` and its slow path; JDK 25's
/// `Arrays.copyOfRange(v, 0, v.length)` is `v.clone()`, so every
/// `StringBuilder.toString()` of a full builder paid it.
///
/// Fast path: a live `byte[]` copied into a bump of the thread's current TLAB
/// -- the arm natives allocate small arrays with
/// (`native_array_tlab_mode`, correct under ZGC, G1 and Generational) -- and
/// its payload copied. Everything else (a null receiver, the TLAB arm off or
/// missing) takes the generic dispatcher, as the site did.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_byte_array_clone_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_byte_array_clone_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_byte_array_clone_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_byte_array_clone_direct`], run under its panic guard.
unsafe fn jit_byte_array_clone_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(copy) = array_clone_serve(vm, receiver) {
        return copy;
    }
    clone_virtual_dispatch(vm_ptr, receiver)
}

/// The cold arm of the virtual `clone()` helpers: the site's call, made by the
/// generic dispatcher ([`CLONE_VIRTUAL_INFO`]).
unsafe fn clone_virtual_dispatch(vm_ptr: i64, receiver: i64) -> i64 {
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &CLONE_VIRTUAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// The array fast path of the `clone()` helpers ([`jit_byte_array_clone_direct`],
/// [`jit_object_clone_virtual_direct`]): a live array of any element type,
/// copied into a bump of the thread's current TLAB -- the arm natives allocate
/// small arrays with (`native_array_tlab_mode`, correct under ZGC, G1 and
/// Generational) -- with the same class word, as `Object.clone`'s native
/// allocates it (`new_array` / `new_ref_array` of the receiver's class id). A
/// pure TLAB bump cannot collect, so the receiver cannot move between the read
/// and the copy. `None` = not served: nothing anything can see (at worst a
/// zeroed, unreferenced array in the TLAB, which the next collection drops).
///
/// j27 wave 3 (lane CLONE): every element type; it served `byte[]` only.
unsafe fn array_clone_serve(vm: &SharedVm, receiver: i64) -> Option<i64> {
    if crate::runtime::env_cache::native_array_tlab_mode()
        == crate::runtime::env_cache::NATIVE_ARRAY_TLAB_OFF
        || clone_vm_object_alloc_armed(vm)
    {
        return None;
    }
    // The receiver is a compiled `clone()` site's own operand, typed by the
    // verifier, so a plausibility screen and the header checks below stand in
    // for a registry lookup (`is_object_address`, a ZGC object-starts probe per
    // call).
    if !cratonvm_types::plausible_heap_pointer(receiver as u64) {
        return None;
    }
    let object = ObjectRef::from_raw(receiver as usize as *mut u8);
    let object = vm.mem.heap.load_and_forward(object);
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let element_type = vm.mem.heap.element_type_of_validated(object);
    let length = vm.mem.heap.array_length(object);
    let class_id = vm.mem.heap.class_id_of(object);
    let (thread, _guard) = jit_thread_mut()?;
    let copy = crate::runtime::interpreter::tlab_alloc_array_native(
        thread,
        vm,
        class_id,
        element_type,
        length,
        false,
    )?;
    array_clone_copy(vm, object, copy, element_type, length)?;
    Some(copy.as_ptr() as i64)
}

/// j27 wave 3 (lane CLONE; parity, no switch): does a JVMTI agent listen for
/// `VMObjectAlloc`? `Object.clone`'s native posts the event for its copy
/// (`vm_object_alloc_event`), and the clone helpers' copies did not, so an
/// agent saw a clone from interpreted code and missed the same clone once
/// compiled. Armed, every clone helper declines to the native. One relaxed
/// load (`NativeContextImpl::jvmti_vm_object_alloc_armed`'s question).
///
/// j27 wave 4 (lane CLONE2; parity, no switch): `SampledObjectAlloc` too. An
/// armed sampler sends every method to the interpreter
/// (`publish_debugger_gates`), but a compiled frame already running (an OSR
/// loop) keeps calling the helper, whose TLAB bump no sampler sees; the
/// native the decline reaches allocates as the interpreter's clone does.
fn clone_vm_object_alloc_armed(vm: &SharedVm) -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        let gates = &vm.debug.debugger_gates;
        gates.native_event_armed(crate::debug::NativeEvent::VmObjectAlloc)
            || gates.native_event_armed(crate::debug::NativeEvent::SampledAlloc)
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = vm;
        false
    }
}

/// [`array_clone_serve`]'s copy of `length` elements of `src` into the fresh
/// `copy`. A primitive payload is copied raw; a reference payload goes through
/// the unchecked reference arraycopy (`try_reference_arraycopy_pair`: same
/// component class, so no store check; the collector's raw-word arm or its
/// per-element accessors, each with the barriers a store carries). `None` =
/// not copied (an auto-box payload the reference copy refuses), `copy`
/// unwritten.
unsafe fn array_clone_copy(
    vm: &SharedVm,
    src: ObjectRef,
    copy: ObjectRef,
    element_type: ArrayElementType,
    length: usize,
) -> Option<()> {
    if element_type == ArrayElementType::Reference {
        return try_reference_arraycopy_pair(vm, src, 0, length, copy, 0, length, length);
    }
    let bytes = length.checked_mul(cratonvm_types::element_byte_size(element_type))?;
    let from = vm.mem.heap.array_data_ptr(src)?;
    let to = vm.mem.heap.array_data_ptr(copy)?;
    // SAFETY: both are the payloads of live primitive arrays of one element
    // type and `length` elements, so `bytes` long; the copy is fresh, so the
    // two cannot overlap.
    std::ptr::copy_nonoverlapping(from as *const u8, to, bytes);
    Some(())
}

/// Synthetic call-site info for the cold arm of [`jit_object_clone_direct`]:
/// the `invokespecial java/lang/Object.clone` it stands for, through the
/// generic dispatcher (which raises the null receiver's NPE and
/// `CloneNotSupportedException`, and serves every receiver the fast path
/// declines).
static OBJECT_CLONE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "clone",
    descriptor: "()Ljava/lang/Object;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct helper for a compiled `super.clone()` that reaches
/// `Object.clone` (`invokespecial java/lang/Object.clone`;
/// `CRATONVM_JIT_OBJECT_CLONE_DIRECT`). The site went through the inline-cache
/// door into the registered native (`safe_native_call`, a `NativeContext`, the
/// class questions, a slot-by-slot copy): ~1.3 us a clone against HotSpot's
/// ~0.1 us, and `TimeZone.getDefault()` clones its default zone per call --
/// once per entry time `jdk.nio.zipfs` converts.
///
/// Fast path ([`object_clone_serve`]): a plain instance whose class is
/// `Cloneable` and needs nothing but its fields copied, into a bump of the
/// thread's TLAB (the arm natives allocate with; it cannot collect), through
/// the heap's one-validation copy. Everything else takes the generic
/// dispatcher, as the site did.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_object_clone_direct(vm_ptr: i64, receiver: i64) -> i64 {
    let f = || jit_object_clone_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_object_clone_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_object_clone_direct`], run under its panic guard.
unsafe fn jit_object_clone_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(copy) = object_clone_serve(vm, receiver) {
        return copy;
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &OBJECT_CLONE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// The fast path of [`jit_object_clone_direct`]; `None` = not served, nothing
/// allocated or written that anything can see.
unsafe fn object_clone_serve(vm: &SharedVm, receiver: i64) -> Option<i64> {
    object_clone_serve_where(vm, receiver, object_clone_is_plain)
}

/// [`object_clone_serve`] for an instance whose class `admits` answers
/// (`object_clone_is_plain` at a `super.clone()` site,
/// `object_clone_virtual_serves` at a virtual one).
unsafe fn object_clone_serve_where(
    vm: &SharedVm,
    receiver: i64,
    admits: fn(&SharedVm, ClassId) -> bool,
) -> Option<i64> {
    // The receiver is a compiled `clone()` site's own operand, typed by the
    // verifier, so a plausibility screen and the header checks below stand in
    // for a registry lookup (as for `byte[].clone()`); the copy validates both
    // objects once more.
    if !cratonvm_types::plausible_heap_pointer(receiver as u64) || clone_vm_object_alloc_armed(vm)
    {
        return None;
    }
    let object = ObjectRef::from_raw(receiver as usize as *mut u8);
    let object = vm.mem.heap.load_and_forward(object);
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Object {
        return None;
    }
    let class_id = vm.mem.heap.class_id_of(object);
    // j27 wave 4 (lane CLONE2): every refusal of the copy that is not about
    // this object is asked here, before the bump -- the class's in `admits`
    // (`OBJECT_CLONE_COMPACT_COPY_BIT`), the heap's per-call state below --
    // so a class the copy refuses declines with nothing allocated.
    if class_id == ClassId::new(0)
        || !admits(vm, class_id)
        || !vm.mem.heap.compact_object_copy_ready()
    {
        return None;
    }
    let slots = vm.mem.heap.get_header(object).num_slots() as usize;
    let (size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
        vm,
        class_id,
        slots,
        crate::runtime::interpreter::tlab_site::NATIVE,
    );
    if size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    // A pure bump (no refill collects), so `object` cannot move before the copy.
    let copy =
        crate::runtime::interpreter::tlab_alloc_object(thread, vm, class_id, slots, size)?;
    // `None` (now only a per-object refusal: a non-compact receiver body)
    // leaves a zeroed, unreferenced object in the TLAB: garbage the next
    // collection drops, and the dispatcher below makes the real clone.
    vm.mem.heap.copy_compact_object_fields(object, copy)?;
    Some(copy.as_ptr() as i64)
}

thread_local! {
    /// [`object_clone_class_verdict`]'s answers: `(vm identity, address
    /// epoch, class id) -> verdict bits`, direct-mapped. A `ClassId` is reused
    /// only after an unload, which happens in a collection and so ends the
    /// epoch (the native's own memo, `object_clone_class_facts`, keys the same
    /// way). A class's methods and superclass chain are fixed once it is
    /// loaded (a redefinition cannot add or remove a method), so the
    /// inheritance bit holds for the same key too.
    static OBJECT_CLONE_PLAIN: std::cell::Cell<[(usize, u64, u32, u8); 16]> =
        const { std::cell::Cell::new([(0, 0, 0, 0); 16]) };
}

/// [`object_clone_class_verdict`]: `Object.clone` of an instance of the class
/// copies its fields and nothing else ([`object_clone_is_plain`]).
const OBJECT_CLONE_PLAIN_BIT: u8 = 1;
/// [`object_clone_class_verdict`]: no class on the chain below `Object`
/// declares `clone()Ljava/lang/Object;` (a covariant override's bridge
/// included), so a virtual `clone()` of an instance selects `Object.clone`.
const OBJECT_CLONE_INHERITED_BIT: u8 = 2;
/// [`object_clone_class_verdict`] (j27 wave 4, lane CLONE2): the heap's
/// compact field copy covers the class (`VmHeap::compact_object_copy_admits_class`).
/// Without it `copy_compact_object_fields` refuses EVERY instance, after the
/// helper had already bumped the copy: a zeroed dead object per clone.
const OBJECT_CLONE_COMPACT_COPY_BIT: u8 = 4;

/// Whether a `super.clone()` of an instance of `class_id` is
/// [`object_clone_serve`]'s copy: `Object.clone` of it is its fields copied
/// and nothing else -- the class is `Cloneable` (else the native throws
/// `CloneNotSupportedException`), not a `Throwable` (the clone keeps its own
/// copy of the trace, `clone_throwable_backtrace`), without a finalizer (the
/// clone is registered), and not a `LinkedHashMap` or `Properties` (their
/// side-table overlays are copied) -- and the compact copy covers the class.
/// A class this cannot answer is not served.
fn object_clone_is_plain(vm: &SharedVm, class_id: ClassId) -> bool {
    const BOTH: u8 = OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_COMPACT_COPY_BIT;
    object_clone_class_verdict(vm, class_id) & BOTH == BOTH
}

/// j27 wave 3 (lane CLONE): whether a VIRTUAL `clone()` of an instance of
/// `class_id` is [`object_clone_serve`]'s copy -- [`object_clone_is_plain`],
/// and the class inherits `Object.clone` (no override on its chain). The
/// one-class answer both [`jit_object_clone_virtual_direct`] (per call) and
/// the compiler's bind (`jit_object_clone_served_class`) read, so the two
/// cannot disagree.
fn object_clone_virtual_serves(vm: &SharedVm, class_id: ClassId) -> bool {
    const ALL: u8 =
        OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_INHERITED_BIT | OBJECT_CLONE_COMPACT_COPY_BIT;
    object_clone_class_verdict(vm, class_id) & ALL == ALL
}

/// The memoised verdict bits of `class_id` (see [`OBJECT_CLONE_PLAIN`]).
fn object_clone_class_verdict(vm: &SharedVm, class_id: ClassId) -> u8 {
    object_clone_class_verdict_by(vm, class_id, || object_clone_compute_verdict(vm, class_id))
}

/// [`object_clone_class_verdict`] with its computation `compute` injected:
/// a memo hit answers without asking; a `None` (the class manager was busy)
/// answers `0` -- not plain, not inherited: the helper declines and the
/// compiler does not bind -- and is NOT remembered, so the next ask computes.
fn object_clone_class_verdict_by(
    vm: &SharedVm,
    class_id: ClassId,
    compute: impl FnOnce() -> Option<u8>,
) -> u8 {
    let key = (vm.vm_identity, vm.mem.address_epoch(), class_id.as_u32());
    let line = (class_id.as_u32() as usize) & 15; // Widening: always safe
    let memo = OBJECT_CLONE_PLAIN.with(|m| m.get()[line]);
    if (memo.0, memo.1, memo.2) == key {
        return memo.3;
    }
    let Some(verdict) = compute() else {
        return 0;
    };
    OBJECT_CLONE_PLAIN.with(|m| {
        let mut lines = m.get();
        lines[line] = (key.0, key.1, key.2, verdict);
        m.set(lines);
    });
    verdict
}

/// [`object_clone_class_verdict`]'s class questions, asked of the class
/// manager, or `None` when it is busy.
///
/// j27 wave 4 (lane CLONE2): never waits. The compiler thread asks this
/// through `jit_object_clone_served_class` at a bind, where a WAITING read is
/// one queued writer away from deadlocking a compile against a class load
/// (the `jit_well_known_class_id` discipline); a Java thread asks it inside a
/// compiled helper with its frame's references unpublished. It was a
/// blocking `read()` on both. `try_read_untracked` adds no wait-for edge, so
/// it stays outside the lock-order graph.
fn object_clone_compute_verdict(vm: &SharedVm, class_id: ClassId) -> Option<u8> {
    let cm = vm.classes.class_manager.try_read_untracked()?;
    let sub = |name: &str| {
        cm.get_loaded_class_id(name)
            .map(|id| cm.is_subclass_of(class_id, id))
    };
    let plain = cm.class_store.get(class_id).is_some_and(|c| !c.has_finalizer)
        && sub("java/lang/Cloneable") == Some(true)
        && sub("java/lang/Throwable") != Some(true)
        && sub("java/util/LinkedHashMap") != Some(true)
        && sub("java/util/Properties") != Some(true);
    let mut verdict = if plain { OBJECT_CLONE_PLAIN_BIT } else { 0 };
    if !class_chain_overrides_clone(&cm, class_id) {
        verdict |= OBJECT_CLONE_INHERITED_BIT;
    }
    if vm.mem.heap.compact_object_copy_admits_class(class_id) {
        verdict |= OBJECT_CLONE_COMPACT_COPY_BIT;
    }
    Some(verdict)
}

/// Does a class on `class_id`'s superclass chain below `java/lang/Object`
/// declare `clone()Ljava/lang/Object;`? A bridge counts: javac emits one for a
/// covariant `Foo clone()`, and a virtual call selects it. A chain this cannot
/// walk to `Object` (an unknown class, a cut chain, more than 64 levels)
/// answers `true`, the refusing answer.
fn class_chain_overrides_clone(cm: &crate::classloading::ClassManager, class_id: ClassId) -> bool {
    let mut c = class_id;
    for _ in 0..64 {
        let Some(class) = cm.class_store.get(c) else {
            return true;
        };
        let Some(parent) = class.superclass else {
            // Only `java/lang/Object` has no superclass: its own `clone`.
            return false;
        };
        if class.find_method("clone", "()Ljava/lang/Object;").is_some() {
            return true;
        }
        c = parent;
    }
    true
}

/// Thin direct helper for a compiled VIRTUAL `clone()` (j27 wave 3, lane
/// CLONE; `CRATONVM_JIT_CLONE_VIRTUAL_THIN`): an array of any element type
/// ([`array_clone_serve`]; `clone()` of an array is `Object.clone`, which
/// HotSpot's compilers expand inline), or an instance whose class is plain and
/// inherits `Object.clone` ([`object_clone_virtual_serves`], then
/// [`object_clone_serve`]'s copy). Such a site took `jit_invoke_virtual_mic`'s
/// slow path into the registered native on every call (an array receiver
/// never sits in a monomorphic inline cache). Everything else -- a null
/// receiver, an override, `CloneNotSupportedException`, a finalizable class,
/// a TLAB that cannot take the copy -- is the site's own virtual call through
/// the generic dispatcher ([`CLONE_VIRTUAL_INFO`]).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_object_clone_virtual_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_object_clone_virtual_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_object_clone_virtual_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_object_clone_virtual_direct`], run under its panic guard.
unsafe fn jit_object_clone_virtual_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(copy) = array_clone_serve(vm, receiver) {
        return copy;
    }
    if let Some(copy) = object_clone_serve_where(vm, receiver, object_clone_virtual_serves) {
        return copy;
    }
    clone_virtual_dispatch(vm_ptr, receiver)
}

/// The compiler's one-class question for the virtual bind
/// (`DirectHelperTable::object_clone_served_class`): `1` when
/// [`jit_object_clone_virtual_direct`] serves an instance of class `class_id`
/// ([`object_clone_virtual_serves`]), else `0`. Compile time only.
///
/// SAFETY: `ctx` is the live `SharedVm` the table was published for.
pub(in crate::jit) unsafe extern "C" fn jit_object_clone_served_class(
    ctx: i64,
    class_id: i64,
) -> i64 {
    let Ok(raw) = u32::try_from(class_id) else {
        return 0;
    };
    if ctx == 0 || raw == 0 {
        return 0;
    }
    // SAFETY: as the function contract.
    let vm = &*(ctx as *const SharedVm);
    i64::from(object_clone_virtual_serves(vm, ClassId::new(raw)))
}

#[cfg(test)]
mod j27_clone_copy_tests {
    use super::*;

    const PRIMITIVES: [ArrayElementType; 8] = [
        ArrayElementType::Boolean,
        ArrayElementType::Byte,
        ArrayElementType::Char,
        ArrayElementType::Short,
        ArrayElementType::Int,
        ArrayElementType::Float,
        ArrayElementType::Long,
        ArrayElementType::Double,
    ];

    fn vm() -> std::sync::Arc<SharedVm> {
        let config = crate::config::VmConfig {
            use_synthetic_jdk: true,
            ..Default::default()
        };
        let vm = std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config));
        *vm.self_arc.write() = Some(std::sync::Arc::downgrade(&vm));
        vm
    }

    fn payload(vm: &SharedVm, arr: ObjectRef, et: ArrayElementType) -> Vec<u8> {
        let bytes = vm.mem.heap.array_length(arr) * cratonvm_types::element_byte_size(et);
        let base = vm.mem.heap.array_data_ptr(arr).expect("a live array has a payload");
        // SAFETY: the payload of the live array `arr`, `bytes` long.
        unsafe { std::slice::from_raw_parts(base as *const u8, bytes).to_vec() }
    }

    /// A fresh `n`-element array of `et` whose payload bytes are a pattern.
    fn patterned(vm: &SharedVm, et: ArrayElementType, n: usize) -> ObjectRef {
        let arr = vm.mem.heap.alloc_array(ClassId::new(0), et, n);
        let bytes = n * cratonvm_types::element_byte_size(et);
        let base = vm.mem.heap.array_data_ptr(arr).expect("a live array has a payload");
        for i in 0..bytes {
            // SAFETY: `i` is inside the payload of the fresh array.
            unsafe { *base.add(i) = (i as u8).wrapping_mul(31).wrapping_add(7) };
        }
        arr
    }

    fn elements(vm: &SharedVm, arr: ObjectRef) -> Vec<String> {
        (0..vm.mem.heap.array_length(arr))
            .map(|i| format!("{:?}", vm.mem.heap.get_array_element(arr, i)))
            .collect()
    }

    /// Every primitive kind, zero length included: the copy holds the whole
    /// payload and the original is unchanged.
    #[test]
    fn j27_every_primitive_array_is_copied_whole_and_the_original_kept() {
        let vm = vm();
        for et in PRIMITIVES {
            for n in [0usize, 1, 7, 64] {
                let src = patterned(&vm, et, n);
                let before = payload(&vm, src, et);
                let copy = vm.mem.heap.alloc_array(ClassId::new(0), et, n);
                // SAFETY: two live arrays of this heap; nothing allocates below.
                let done = unsafe { array_clone_copy(&vm, src, copy, et, n) };
                assert_eq!(done, Some(()), "{et:?} x{n}");
                assert_eq!(payload(&vm, copy, et), before, "{et:?} x{n}: the copy");
                assert_eq!(payload(&vm, src, et), before, "{et:?} x{n}: the original");
            }
        }
    }

    /// A reference array, a null slot included: the copy names the same
    /// objects in the same slots, and the original still does.
    #[test]
    fn j27_a_reference_array_is_copied_slot_for_slot() {
        let vm = vm();
        let component = ClassId::new(7);
        let objs: Vec<ObjectRef> = (0..5).map(|_| vm.mem.heap.alloc_object(component, 0)).collect();
        let src = vm.mem.heap.alloc_array(component, ArrayElementType::Reference, 6);
        for (i, o) in objs.iter().enumerate() {
            let _ = vm.mem.heap.set_array_element(src, i, Value::Object(Some(*o)));
        }
        let before = elements(&vm, src);
        let copy = vm.mem.heap.alloc_array(component, ArrayElementType::Reference, 6);
        // SAFETY: two live reference arrays of this heap; nothing allocates below.
        let done = unsafe { array_clone_copy(&vm, src, copy, ArrayElementType::Reference, 6) };
        assert_eq!(done, Some(()));
        assert_eq!(elements(&vm, copy), before);
        assert_eq!(elements(&vm, src), before);
        assert_eq!(before[5], format!("{:?}", vm.mem.heap.get_array_element(copy, 5)));
    }

    /// The declines are the site's own VIRTUAL call (an override must be
    /// selected), and the compiler's question answers nothing it cannot ask.
    #[test]
    fn j27_the_decline_is_virtual_and_the_question_refuses_no_vm_or_object() {
        assert_eq!(CLONE_VIRTUAL_INFO.invoke_kind, 0);
        assert_eq!(
            (CLONE_VIRTUAL_INFO.class_name, CLONE_VIRTUAL_INFO.method_name),
            ("java/lang/Object", "clone")
        );
        let vm = vm();
        let ctx = &*vm as *const SharedVm as i64;
        // SAFETY: `ctx` is the live VM above, or 0.
        unsafe {
            assert_eq!(jit_object_clone_served_class(0, 9), 0);
            assert_eq!(jit_object_clone_served_class(ctx, 0), 0);
            assert_eq!(jit_object_clone_served_class(ctx, -1), 0);
        }
    }

    /// j27 wave 4 (lane CLONE2): the compiler's question never waits for the
    /// class manager. With the write side held (a class load in flight) it
    /// answers "not served" at once; it used to block in `read()` for as long
    /// as the writer held it (here, this very thread: forever).
    #[test]
    fn j27_a_busy_class_manager_answers_not_served_without_waiting() {
        let vm = vm();
        let ctx = &*vm as *const SharedVm as i64;
        let held = vm.classes.class_manager.write();
        // SAFETY: `ctx` is the live VM above.
        let answer = unsafe { jit_object_clone_served_class(ctx, 0x4d1) };
        drop(held);
        assert_eq!(answer, 0);
    }

    /// A busy answer is `0` and is not remembered; a computed one is, and a
    /// later ask does not compute again.
    #[test]
    fn j27_a_busy_verdict_is_not_memoised_and_a_computed_one_is() {
        const BOTH: u8 = OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_INHERITED_BIT;
        let vm = vm();
        let class = ClassId::new(0x4d2);
        assert_eq!(object_clone_class_verdict_by(&vm, class, || None), 0);
        assert_eq!(object_clone_class_verdict_by(&vm, class, || Some(BOTH)), BOTH);
        let again = object_clone_class_verdict_by(&vm, class, || panic!("asked twice"));
        assert_eq!(again, BOTH);
    }

    /// j27 wave 4 (lane CLONE2): a class the compact copy does not cover is
    /// served by neither clone helper, however plain and inherited it is, so
    /// both decline before the bump; with the bit it is served by both.
    #[test]
    fn j27_a_class_the_compact_copy_refuses_is_not_served() {
        const PLAIN_INHERITED: u8 = OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_INHERITED_BIT;
        let vm = vm();
        let (refused, covered) = (ClassId::new(0x4d3), ClassId::new(0x4d4));
        // Seed the memo with each class's verdict (see the test above).
        object_clone_class_verdict_by(&vm, refused, || Some(PLAIN_INHERITED));
        object_clone_class_verdict_by(&vm, covered, || {
            Some(PLAIN_INHERITED | OBJECT_CLONE_COMPACT_COPY_BIT)
        });
        assert!(!object_clone_is_plain(&vm, refused));
        assert!(!object_clone_virtual_serves(&vm, refused));
        assert!(object_clone_is_plain(&vm, covered));
        assert!(object_clone_virtual_serves(&vm, covered));
    }
}

/// The `StringBuilder` append intrinsics' growth (strings round 2, `str17`;
/// `CRATONVM_JIT_SB_GROW`), called by the stub `emit_sb_grow_stub` emits on a
/// full LATIN1 payload. The decline edge it replaces was the generic
/// `jit_invoke_dispatch` into the JDK's `append` and from there its compiled
/// growth chain (`ensureCapacitySameCoder`, `newCapacity`, `Arrays.copyOf`):
/// ~2 400 cycles, on 29% of `R16Bench strings`' iterations.
///
/// Installs in `value` -- the reference slot `value_offset` bytes into the
/// builder, the address the inline body itself reads, passed per layout by
/// the stub -- a copy grown exactly as JDK 25's `newCapacity` grows a LATIN1
/// payload: `ArraysSupport.newLength(old, min - old, old + 2)`, i.e.
/// `max(min, 2 * old + 2)` below `SOFT_MAX_ARRAY_LENGTH`. Returns the builder;
/// the stub then runs the inline body again, which finds room and writes the
/// characters and `count`. `capacity()` observes the rule, so it is the JDK's
/// and nothing else. `maybeLatin1` is written only on UTF16 and shrinking
/// paths, so a LATIN1 growth leaves it alone, as here.
///
/// `0` = not served, nothing written: the stub takes the decline edge as
/// before. That covers a `hugeLength` size, a payload already large enough
/// (so a disagreement can never loop the stub), a slot that is not a `byte[]`,
/// an armed ZGC load barrier (colored slots), and a TLAB that cannot take the
/// copy -- a pure bump, the arm natives
/// allocate small arrays with, so nothing here can collect and the stub needs
/// no safepoint.
///
/// The builder is the inline body's own receiver, already null-checked and
/// guarded on its exact class, so it is not looked up again; the slot is
/// written as compiled code writes a reference field: the collector's SATB
/// pre-barrier on the old payload, the store, its post barrier on the new one
/// (`jit_satb_pre_write_barrier` / `jit_write_barrier`'s own calls).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_sb_grow_direct(
    vm_ptr: i64,
    builder: i64,
    value_offset: i64,
    min_capacity: i64,
) -> i64 {
    let f = || sb_grow_serve(vm_ptr, builder, value_offset, min_capacity).unwrap_or(0);
    let bits = contain("jit_sb_grow_direct", OnPanic::RecordFor { vm_ptr }, 0, f);
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// The body of [`jit_sb_grow_direct`]; `None` = not served.
unsafe fn sb_grow_serve(
    vm_ptr: i64,
    builder: i64,
    value_offset: i64,
    min_capacity: i64,
) -> Option<i64> {
    // `ArraysSupport.SOFT_MAX_ARRAY_LENGTH`: past it `newLength` takes its
    // `hugeLength` arm, which this does not reproduce.
    const SOFT_MAX: usize = i32::MAX as usize - 8;
    crate::jit::conservative_roots::note_jit_boundary();
    if crate::runtime::env_cache::native_array_tlab_mode()
        == crate::runtime::env_cache::NATIVE_ARRAY_TLAB_OFF
        || cratonvm_types::narrow_oop::narrow_oops_enabled()
    {
        return None;
    }
    let min = usize::try_from(min_capacity)
        .ok()
        .filter(|&m| m > 0 && m <= SOFT_MAX)?;
    // An 8-byte reference slot inside the object, naturally aligned, as the
    // layout the stub was compiled against places it.
    let offset = usize::try_from(value_offset)
        .ok()
        .filter(|&o| o >= 8 && o < 4096 && o % 8 == 0)?;
    if !cratonvm_types::plausible_heap_pointer(builder as u64) {
        return None;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.mem.heap;
    // zt t33: while a ZGC load barrier is armed the slot holds a colored word
    // the raw read below would not heal, and the raw store would leave a
    // plain word no barrier ever forwards (the word R6's plan used to walk
    // the live set for). The decline edge stores through `putfield`.
    if heap.zgc_load_barrier_armed() {
        return None;
    }
    let slot = (builder as usize as *mut u8).add(offset);
    // SAFETY: `slot` is the builder's `value` reference slot (see above).
    let old_raw = read_ref_slot(slot);
    if !cratonvm_types::plausible_heap_pointer(old_raw) {
        return None;
    }
    let old = ObjectRef::from_raw(old_raw as usize as *mut u8);
    if heap.kind_of(old) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(old) != ArrayElementType::Byte
    {
        return None;
    }
    let old_len = heap.array_length(old);
    if min <= old_len {
        return None;
    }
    let new_len = std::cmp::max(min, old_len.checked_mul(2)?.checked_add(2)?);
    if new_len > SOFT_MAX {
        return None;
    }
    let class_id = heap.class_id_of(old);
    let (thread, _guard) = jit_thread_mut()?;
    let grown = crate::runtime::interpreter::tlab_alloc_array_native(
        thread,
        vm,
        class_id,
        ArrayElementType::Byte,
        new_len,
        false,
    )?;
    let src = heap.array_data_ptr(old)?;
    let dst = heap.array_data_ptr(grown)?;
    // SAFETY: `src` is the payload of a live `byte[]` of `old_len` elements
    // and `dst` that of a fresh one of `new_len > old_len`; they cannot
    // overlap. The tail is zeroed as `Arrays.copyOf` leaves it.
    std::ptr::copy_nonoverlapping(src as *const u8, dst, old_len);
    std::ptr::write_bytes(dst.add(old_len), 0, new_len - old_len);
    // zt t32 (2f): the slot-aware form.
    heap.satb_barrier_slot(slot as *const u8, Value::Object(Some(old)));
    // SAFETY: the same slot, read above.
    write_ref_slot(slot, grown.as_ptr() as u64);
    heap.write_barrier(
        ObjectRef::from_raw(builder as usize as *mut u8),
        Value::Object(Some(grown)),
    );
    Some(builder)
}

/// Synthetic call-site info for the cold arm of
/// [`jit_vectorized_hash_code_direct`]: the registered native, through the
/// generic dispatcher.
static VECTORIZED_HASH_CODE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/ArraysSupport",
    method_name: "vectorizedHashCode",
    descriptor: "(Ljava/lang/Object;IIII)I",
    num_jit_args: 5,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// HotSpot `BasicType` codes `ArraysSupport.vectorizedHashCode` switches on.
const VH_T_BOOLEAN: i64 = 4;
const VH_T_CHAR: i64 = 5;
const VH_T_BYTE: i64 = 8;
const VH_T_SHORT: i64 = 9;
const VH_T_INT: i64 = 10;

/// Thin direct helper for `jdk/internal/util/ArraysSupport.vectorizedHashCode
/// (Object,IIII)I` (strings round 2, `str17`;
/// `CRATONVM_JIT_VECTORIZED_HASH_DIRECT`). The callee is a registered
/// `Intrinsic` native, so compiled code reached it through
/// `jit_invoke_dispatch` -- argument decode, the native funnel, a `Value` per
/// argument -- on every `String.hashCode()` of a string not hashed before:
/// ~1 500 cycles for a 17-byte payload, most of `R16Bench strings`.
///
/// Fast path: `array` a live primitive array whose element type is the one
/// `basicType` names (or a `byte[]` of UTF-16 pairs for `T_CHAR`, as
/// `StringUTF16.hashCode` passes it) and `[fromIndex, fromIndex + length)`
/// inside it; the fold is the JDK body's (`31 * h + element`, the element
/// widened as that body widens it). Everything else falls through to the
/// generic dispatcher, so the native decides it exactly as it does today.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_vectorized_hash_code_direct(
    vm_ptr: i64,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i64,
    basic_type: i64,
) -> i64 {
    let f = || {
        jit_vectorized_hash_code_direct_body(
            vm_ptr,
            array,
            from_index,
            length,
            initial_value,
            basic_type,
        )
    };
    contain(
        "jit_vectorized_hash_code_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_vectorized_hash_code_direct`], run under its panic guard.
unsafe fn jit_vectorized_hash_code_direct_body(
    vm_ptr: i64,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i64,
    basic_type: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // The three `int` arguments arrive sign-extended; read them as `int`s.
    let (from_index, length) = (i64::from(from_index as i32), i64::from(length as i32));
    let basic_type = i64::from(basic_type as i32);
    if let Some(hash) = vectorized_hash_code_serve(
        vm,
        array,
        from_index,
        length,
        initial_value as i32,
        basic_type,
    ) {
        return i64::from(hash);
    }
    let args = [array, from_index, length, initial_value, basic_type];
    jit_invoke_dispatch(
        vm_ptr,
        &VECTORIZED_HASH_CODE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, opt-in): the LEAF TWIN of
/// [`jit_vectorized_hash_code_direct`]: its fast arm verbatim
/// (`vectorized_hash_code_serve`: a validated primitive array's payload folded
/// in place, "no allocation, no safepoint"), and
/// `cratonvm_jit::direct_helpers::LEAF_TWIN_DECLINE_OUTSIDE_INT` -- outside the
/// sign-extended `int` range every hash lies in -- where the helper would
/// dispatch the registered native. The contract is
/// [`jit_preconditions_check_index_leaf`]'s.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_vectorized_hash_code_leaf(
    vm_ptr: i64,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i64,
    basic_type: i64,
) -> i64 {
    const DECLINE: i64 = cratonvm_jit::direct_helpers::LEAF_TWIN_DECLINE_OUTSIDE_INT;
    let f = || {
        // SAFETY: as in `jit_vectorized_hash_code_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        // The three `int` arguments arrive sign-extended; read them as `int`s.
        let (from_index, length) = (i64::from(from_index as i32), i64::from(length as i32));
        let basic_type = i64::from(basic_type as i32);
        match vectorized_hash_code_serve(
            vm,
            array,
            from_index,
            length,
            initial_value as i32,
            basic_type,
        ) {
            Some(hash) => i64::from(hash),
            None => DECLINE,
        }
    };
    contain("jit_vectorized_hash_code_leaf", OnPanic::RecordFor { vm_ptr }, DECLINE, f)
}

/// The fast path of [`jit_vectorized_hash_code_direct`]: the same validation
/// `decimal_digits_latin1_serve` makes (aligned, heap-resident, an array of
/// the expected element type), then [`vectorized_hash_fold`] over the range.
unsafe fn vectorized_hash_code_serve(
    vm: &SharedVm,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i32,
    basic_type: i64,
) -> Option<i32> {
    if from_index < 0 || length < 0 {
        return None;
    }
    let raw = array as u64;
    if array == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    // `(payload bytes per array slot, payload bytes per hashed element)`.
    let (width, step) = match (vm.mem.heap.element_type_of_validated(object), basic_type) {
        (ArrayElementType::Byte, VH_T_BOOLEAN | VH_T_BYTE) => (1usize, 1usize),
        (ArrayElementType::Byte, VH_T_CHAR) => (1, 2),
        (ArrayElementType::Char, VH_T_CHAR) | (ArrayElementType::Short, VH_T_SHORT) => (2, 2),
        (ArrayElementType::Int, VH_T_INT) => (4, 4),
        _ => return None,
    };
    let payload_len = vm.mem.heap.array_length(object).checked_mul(width)?;
    let start = (from_index as usize).checked_mul(step)?;
    let end = start.checked_add((length as usize).checked_mul(step)?)?;
    if end > payload_len {
        return None;
    }
    let base = vm.mem.heap.array_data_ptr(object)?;
    // SAFETY: `base` is the payload of a live primitive array of
    // `payload_len` bytes, and nothing between here and the fold can move or
    // free it (no allocation, no safepoint).
    let payload = std::slice::from_raw_parts(base as *const u8, payload_len);
    if vectorized_hash_unroll_enabled(vm) {
        return vectorized_hash_fold_unrolled(&payload[start..end], step, basic_type, initial_value);
    }
    vectorized_hash_fold(&payload[start..end], step, basic_type, initial_value)
}

/// `CRATONVM_JIT_VECTORIZED_HASH_UNROLL` (JIT round 21 wave 3, lane
/// intrinsics, INTRINSICS21-6; perf, **opt-in**): the direct
/// `ArraysSupport.vectorizedHashCode` helper folds four elements per step
/// ([`vectorized_hash_fold_unrolled`]) instead of one. Read once per VM per
/// thread (the helper serves every compiled `String.hashCode` /
/// `Arrays.hashCode` miss).
fn vectorized_hash_unroll_enabled(vm: &SharedVm) -> bool {
    thread_local! {
        static ON: std::cell::Cell<Option<(usize, bool)>> = const { std::cell::Cell::new(None) };
    }
    let id = vm.vm_identity;
    ON.with(|c| match c.get() {
        Some((owner, on)) if owner == id => on,
        _ => {
            let on =
                cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_VECTORIZED_HASH_UNROLL");
            c.set(Some((id, on)));
            on
        }
    })
}

/// `h = 31 * h + e(i)` over `n` elements, four per step:
/// `h' = 31^4 h + 31^3 e0 + 31^2 e1 + 31 e2 + e3`. The same value as the
/// one-at-a-time fold for every input -- wrapping `i32` arithmetic is a ring,
/// so the expansion is exact mod 2^32 -- with ONE multiply-add on the loop-
/// carried chain per four elements instead of four (HotSpot's AVX2
/// `vectorizedHashCode` stub uses the same power table, eight lanes wide).
fn hash_fold_unrolled4(n: usize, initial_value: i32, elem: impl Fn(usize) -> i32) -> i32 {
    const P2: i32 = 31 * 31;
    const P3: i32 = 31 * 31 * 31;
    const P4: i32 = 31 * 31 * 31 * 31;
    let mut h = initial_value;
    let mut i = 0usize;
    while i + 4 <= n {
        h = h
            .wrapping_mul(P4)
            .wrapping_add(elem(i).wrapping_mul(P3))
            .wrapping_add(elem(i + 1).wrapping_mul(P2))
            .wrapping_add(elem(i + 2).wrapping_mul(31))
            .wrapping_add(elem(i + 3));
        i += 4;
    }
    while i < n {
        h = h.wrapping_mul(31).wrapping_add(elem(i));
        i += 1;
    }
    h
}

/// [`vectorized_hash_fold`]'s answer through [`hash_fold_unrolled4`]: the
/// same element widening per `(step, basic_type)`, the same `None` for a pair
/// the JDK never passes, and the same whole-element count (a trailing partial
/// element is ignored, as `chunks_exact` ignores it).
fn vectorized_hash_fold_unrolled(
    bytes: &[u8],
    step: usize,
    basic_type: i64,
    initial_value: i32,
) -> Option<i32> {
    let n = bytes.len() / step.max(1);
    Some(match (step, basic_type) {
        (1, VH_T_BOOLEAN) => hash_fold_unrolled4(n, initial_value, |i| i32::from(bytes[i])),
        (1, VH_T_BYTE) => hash_fold_unrolled4(n, initial_value, |i| i32::from(bytes[i] as i8)),
        (2, VH_T_CHAR) => hash_fold_unrolled4(n, initial_value, |i| {
            i32::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
        }),
        (2, VH_T_SHORT) => hash_fold_unrolled4(n, initial_value, |i| {
            i32::from(i16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
        }),
        (4, VH_T_INT) => hash_fold_unrolled4(n, initial_value, |i| {
            i32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]])
        }),
        _ => return None,
    })
}

/// `ArraysSupport.vectorizedHashCode`'s fold over `bytes` (whole elements of
/// `step` bytes, little-endian): `h = 31 * h + element`, the element widened
/// as the JDK body widens it -- `T_BOOLEAN` an unsigned byte
/// (`unsignedHashCode`), `T_BYTE` a signed byte, `T_CHAR` an unsigned
/// 16-bit unit, `T_SHORT` a signed one, `T_INT` the word. `None` for a
/// `(step, basic_type)` pair the JDK never passes.
fn vectorized_hash_fold(
    bytes: &[u8],
    step: usize,
    basic_type: i64,
    initial_value: i32,
) -> Option<i32> {
    let mut h = initial_value;
    match (step, basic_type) {
        (1, VH_T_BOOLEAN) => {
            for &b in bytes {
                h = h.wrapping_mul(31).wrapping_add(i32::from(b));
            }
        }
        (1, VH_T_BYTE) => {
            for &b in bytes {
                h = h.wrapping_mul(31).wrapping_add(i32::from(b as i8));
            }
        }
        (2, VH_T_CHAR) => {
            for c in bytes.chunks_exact(2) {
                h = h
                    .wrapping_mul(31)
                    .wrapping_add(i32::from(u16::from_le_bytes([c[0], c[1]])));
            }
        }
        (2, VH_T_SHORT) => {
            for c in bytes.chunks_exact(2) {
                h = h
                    .wrapping_mul(31)
                    .wrapping_add(i32::from(i16::from_le_bytes([c[0], c[1]])));
            }
        }
        (4, VH_T_INT) => {
            for c in bytes.chunks_exact(4) {
                h = h
                    .wrapping_mul(31)
                    .wrapping_add(i32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        _ => return None,
    }
    Some(h)
}

/// Synthetic call-site info for the cold arm of
/// [`jit_vectorized_mismatch_direct`]: the registered native, through the
/// generic dispatcher. A `const`, not a `static` (the per-VM statics ratchet):
/// the one `&` below is promoted to a single immutable allocation.
const VECTORIZED_MISMATCH_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/ArraysSupport",
    method_name: "vectorizedMismatch",
    descriptor: "(Ljava/lang/Object;JLjava/lang/Object;JII)I",
    num_jit_args: 6,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// `Unsafe.arrayBaseOffset` of every primitive array in this VM
/// (`native_unsafe_array_base_offset`): the JDK passes `vectorizedMismatch`
/// offsets measured from the object as if its header were HotSpot's 16 bytes.
const VECTORIZED_MISMATCH_UNSAFE_ABASE: i64 = 16;

/// Thin direct helper for `jdk/internal/util/ArraysSupport.vectorizedMismatch
/// (Object,long,Object,long,int,int)int` (JIT round 22 wave 1, lane intrinsics,
/// INTRINSICS21-1; `CRATONVM_JIT_VECTORIZED_MISMATCH_DIRECT`, opt-in). The
/// callee is a registered `Intrinsic` native, so compiled code reached it
/// through `jit_invoke_dispatch` on every `Arrays.equals` / `mismatch` /
/// `compare` range and every buffer compare.
///
/// Fast path ([`vectorized_mismatch_serve`]): two live on-heap primitive arrays
/// whose element width is `1 << log2ArrayIndexScale`, element-aligned Unsafe
/// offsets at or past the base, `length > 0` elements inside both. The answer
/// is the index of the first element whose BYTES differ, or `-1` -- HotSpot's
/// memory compare, and the native's strict answer (a float / double pair
/// differs exactly when its bits do). Everything else -- a null base (off-heap
/// memory), a typed view over a `byte[]`, a range the helper cannot prove,
/// `length <= 0` -- falls through to the generic dispatcher, so the native
/// decides it exactly as it does today.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit) unsafe extern "C" fn jit_vectorized_mismatch_direct(
    vm_ptr: i64,
    a: i64,
    a_offset: i64,
    b: i64,
    b_offset: i64,
    length: i64,
    log2_scale: i64,
) -> i64 {
    let f = || {
        jit_vectorized_mismatch_direct_body(vm_ptr, a, a_offset, b, b_offset, length, log2_scale)
    };
    contain(
        "jit_vectorized_mismatch_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_vectorized_mismatch_direct`], run under its panic guard.
#[allow(clippy::too_many_arguments)]
unsafe fn jit_vectorized_mismatch_direct_body(
    vm_ptr: i64,
    a: i64,
    a_offset: i64,
    b: i64,
    b_offset: i64,
    length: i64,
    log2_scale: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // The two `int` arguments arrive sign-extended; read them as `int`s.
    let (length32, log2_32) = (i64::from(length as i32), i64::from(log2_scale as i32));
    if let Some(answer) =
        vectorized_mismatch_serve(vm, a, a_offset, b, b_offset, length32, log2_32)
    {
        return i64::from(answer);
    }
    let args = [a, a_offset, b, b_offset, length, log2_scale];
    jit_invoke_dispatch(
        vm_ptr,
        &VECTORIZED_MISMATCH_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// The payload of `array` when it is a live on-heap primitive array whose
/// element width is `scale`: `(first payload byte, payload bytes)`. `None`
/// for anything else (null, misaligned or non-heap word, a reference array, a
/// width mismatch -- the typed-view shape the native's raw-bytes path owns).
unsafe fn vectorized_mismatch_payload(
    vm: &SharedVm,
    array: i64,
    scale: usize,
) -> Option<(*const u8, usize)> {
    let raw = array as u64;
    if array == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let width = match vm.mem.heap.element_type_of_validated(object) {
        ArrayElementType::Boolean | ArrayElementType::Byte => 1usize,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        ArrayElementType::Reference => return None,
    };
    if width != scale {
        return None;
    }
    let bytes = vm.mem.heap.array_length(object).checked_mul(width)?;
    let base = vm.mem.heap.array_data_ptr(object)?;
    Some((base as *const u8, bytes))
}

/// The fast path of [`jit_vectorized_mismatch_direct`]: `Some(index | -1)` for
/// the shape it serves (see that function), `None` to hand the call to the
/// native.
unsafe fn vectorized_mismatch_serve(
    vm: &SharedVm,
    a: i64,
    a_offset: i64,
    b: i64,
    b_offset: i64,
    length: i64,
    log2_scale: i64,
) -> Option<i32> {
    if length <= 0 || !(0..=3).contains(&log2_scale) {
        return None;
    }
    let scale = 1usize << log2_scale;
    // An Unsafe offset at or past the base and a whole number of elements past
    // it; anything else is a shape no JDK caller passes, and the native decides.
    let payload_start = |off: i64| -> Option<usize> {
        let rel = usize::try_from(off.checked_sub(VECTORIZED_MISMATCH_UNSAFE_ABASE)?).ok()?;
        (rel % scale == 0).then_some(rel)
    };
    let a_start = payload_start(a_offset)?;
    let b_start = payload_start(b_offset)?;
    let (a_base, a_bytes) = vectorized_mismatch_payload(vm, a, scale)?;
    let (b_base, b_bytes) = vectorized_mismatch_payload(vm, b, scale)?;
    let n_bytes = usize::try_from(length).ok()?.checked_mul(scale)?;
    if a_start.checked_add(n_bytes)? > a_bytes || b_start.checked_add(n_bytes)? > b_bytes {
        return None;
    }
    // SAFETY: both ranges lie inside the payload of a live primitive array
    // (checked just above), and nothing between the validation and the
    // compare can move or free either array (no allocation, no safepoint).
    let x = std::slice::from_raw_parts(a_base.add(a_start), n_bytes);
    let y = std::slice::from_raw_parts(b_base.add(b_start), n_bytes);
    Some(match first_differing_byte(x, y) {
        // `n_bytes / scale == length`, an `int`: the index fits.
        Some(at) => i32::try_from(at / scale).ok()?,
        None => -1,
    })
}

/// The index of the first byte at which `x` and `y` differ (over the shorter
/// length), or `None`: eight bytes per step (the lowest set bit of the XOR of
/// two little-endian words is the first differing byte), then one at a time.
fn first_differing_byte(x: &[u8], y: &[u8]) -> Option<usize> {
    let n = x.len().min(y.len());
    let mut i = 0usize;
    while i + 8 <= n {
        let mut wx = [0u8; 8];
        let mut wy = [0u8; 8];
        wx.copy_from_slice(&x[i..i + 8]);
        wy.copy_from_slice(&y[i..i + 8]);
        let d = u64::from_le_bytes(wx) ^ u64::from_le_bytes(wy);
        if d != 0 {
            return Some(i + (d.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < n {
        if x[i] != y[i] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Synthetic call-site infos for the cold arms of the three zip-checksum thin
/// helpers below: each names its own site's method, so a decline runs exactly
/// what the site would have run (the JDK's range-checking `CRC32.updateBytes`
/// included). `const`s, not `static`s (the per-VM statics ratchet).
const CRC32_UPDATE_BYTES0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/zip/CRC32",
    method_name: "updateBytes0",
    descriptor: "(I[BII)I",
    num_jit_args: 4,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};
const CRC32_UPDATE_BYTES_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/zip/CRC32",
    method_name: "updateBytes",
    descriptor: "(I[BII)I",
    num_jit_args: 4,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};
const ADLER32_UPDATE_BYTES_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/zip/Adler32",
    method_name: "updateBytes",
    descriptor: "(I[BII)I",
    num_jit_args: 4,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct helper for `java/util/zip/CRC32.updateBytes0(I[BII)I` (JIT
/// round 22 wave 2, lane intrinsics, INTRINSICS22-2;
/// `CRATONVM_JIT_ZIP_CHECKSUM_DIRECT`, opt-in). Fast path
/// ([`zip_checksum_payload`]): `b` a live `byte[]` and `[off, off + len)`
/// inside it; the answer is zlib's CRC-32 over that range in the JDK's public
/// representation (`cratonvm_native_builtins::zip_real::zlib_crc32`, the
/// native's own function), with no copy and no native funnel. Everything else
/// goes to the registered native through the generic dispatcher.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_crc32_update_bytes0_direct(
    vm_ptr: i64,
    crc: i64,
    b: i64,
    off: i64,
    len: i64,
) -> i64 {
    let f = || {
        jit_zip_checksum_direct_body(vm_ptr, crc, b, off, len, &CRC32_UPDATE_BYTES0_INFO, false)
    };
    contain("jit_crc32_update_bytes0_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// [`jit_crc32_update_bytes0_direct`] for the `CRC32.updateBytes` wrapper
/// (registered over the JDK's range-checking body in `--compatible`): the
/// same answer for an in-range call; any other call is the site's own method.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_crc32_update_bytes_direct(
    vm_ptr: i64,
    crc: i64,
    b: i64,
    off: i64,
    len: i64,
) -> i64 {
    let f = || {
        jit_zip_checksum_direct_body(vm_ptr, crc, b, off, len, &CRC32_UPDATE_BYTES_INFO, false)
    };
    contain("jit_crc32_update_bytes_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// [`jit_crc32_update_bytes0_direct`]'s Adler-32 twin for
/// `java/util/zip/Adler32.updateBytes(I[BII)I`
/// (`cratonvm_native_builtins::zip_real::zlib_adler32`).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_adler32_update_bytes_direct(
    vm_ptr: i64,
    adler: i64,
    b: i64,
    off: i64,
    len: i64,
) -> i64 {
    let f = || {
        jit_zip_checksum_direct_body(vm_ptr, adler, b, off, len, &ADLER32_UPDATE_BYTES_INFO, true)
    };
    contain("jit_adler32_update_bytes_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of the three zip-checksum helpers, run under their panic guards.
unsafe fn jit_zip_checksum_direct_body(
    vm_ptr: i64,
    checksum: i64,
    b: i64,
    off: i64,
    len: i64,
    info: &'static JitInvokeInfo,
    adler: bool,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // The three `int` arguments arrive sign-extended; read them as `int`s.
    let sum = checksum as i32 as u32;
    if let Some(bytes) = zip_checksum_payload(vm, b, off as i32, len as i32) {
        let v = if adler {
            cratonvm_native_builtins::zip_real::zlib_adler32(sum, bytes)
        } else {
            cratonvm_native_builtins::zip_real::zlib_crc32(sum, bytes)
        };
        // The `int` result, sign-extended as every compiled `int` is.
        return i64::from(v as i32);
    }
    let args = [checksum, b, off, len];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

/// `b[off..off + len]` when `b` is a live `byte[]` (the validation
/// `decimal_digits_latin1_serve` makes) and the range lies inside it; `None`
/// otherwise (null, not a `byte[]`, a negative `off` / `len`, past the end).
/// The slice stays valid while the caller runs no allocation and no safepoint.
unsafe fn zip_checksum_payload<'a>(
    vm: &'a SharedVm,
    b: i64,
    off: i32,
    len: i32,
) -> Option<&'a [u8]> {
    let raw = b as u64;
    if b == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array
        || vm.mem.heap.element_type_of_validated(object) != ArrayElementType::Byte
    {
        return None;
    }
    let off = usize::try_from(off).ok()?;
    let len = usize::try_from(len).ok()?;
    if off.checked_add(len)? > vm.mem.heap.array_length(object) {
        return None;
    }
    let base = vm.mem.heap.array_data_ptr(object)?;
    // SAFETY: `[off, off + len)` lies inside the payload of a live `byte[]`
    // (checked just above).
    Some(std::slice::from_raw_parts((base as *const u8).add(off), len))
}

#[cfg(test)]
mod r22_intrinsics_vectorized_mismatch_tests {
    use super::*;

    /// The word step finds the same first differing byte as a plain scan, at
    /// every position and every length around the 8-byte boundary.
    #[test]
    fn r22_first_differing_byte_matches_a_byte_scan() {
        for len in 0usize..41 {
            let x: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(first_differing_byte(&x, &x), None, "len {len}");
            for at in 0..len {
                let mut y = x.clone();
                y[at] ^= 0x40;
                if at + 1 < len {
                    y[len - 1] ^= 0x01; // a later difference must not win
                }
                assert_eq!(first_differing_byte(&x, &y), Some(at), "len {len} at {at}");
            }
        }
        // The shorter length bounds the compare.
        assert_eq!(first_differing_byte(&[1, 2, 3], &[1, 2]), None);
    }
}

#[cfg(test)]
mod str17_vectorized_hash_tests {
    use super::*;

    /// `String.hashCode()` of `s` as the JLS defines it.
    fn java_string_hash(s: &str) -> i32 {
        s.encode_utf16()
            .fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)))
    }

    /// The fold answers `String.hashCode()` for both string codings
    /// (`StringLatin1` passes `T_BOOLEAN`, `StringUTF16` a `byte[]` of LE
    /// pairs with `T_CHAR`), `Arrays.hashCode` for the signed element types
    /// (seeded with 1), and refuses a pair the JDK never passes.
    #[test]
    fn str17_the_fold_answers_the_jdk_bodies() {
        let latin1 = "k1999999-13999993\u{e9}";
        let bytes: Vec<u8> = latin1.chars().map(|c| c as u32 as u8).collect();
        assert_eq!(
            vectorized_hash_fold(&bytes, 1, VH_T_BOOLEAN, 0),
            Some(java_string_hash(latin1))
        );
        let utf16 = "\u{3a3}\u{39f}\u{3a3}k";
        let pairs: Vec<u8> = utf16.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(
            vectorized_hash_fold(&pairs, 2, VH_T_CHAR, 0),
            Some(924_359 * 31 + 107)
        );
        assert_eq!(
            vectorized_hash_fold(&pairs, 2, VH_T_CHAR, 0),
            Some(java_string_hash(utf16))
        );
        // `Arrays.hashCode(byte[]{-1, 2})`: signed, seeded with 1.
        assert_eq!(
            vectorized_hash_fold(&[0xff, 2], 1, VH_T_BYTE, 1),
            Some((31 + -1) * 31 + 2)
        );
        // `Arrays.hashCode(short[]{-2})` and `int[]{i32::MIN}`.
        assert_eq!(
            vectorized_hash_fold(&(-2i16).to_le_bytes(), 2, VH_T_SHORT, 1),
            Some(31 - 2)
        );
        assert_eq!(
            vectorized_hash_fold(&i32::MIN.to_le_bytes(), 4, VH_T_INT, 1),
            Some(31i32.wrapping_add(i32::MIN))
        );
        assert_eq!(vectorized_hash_fold(&[], 1, VH_T_BOOLEAN, 7), Some(7));
        assert_eq!(vectorized_hash_fold(&[1, 2], 2, VH_T_INT, 1), None);
    }
}

#[cfg(test)]
mod r16_strings_decimal_digits_tests {
    use super::decimal_digits_latin1_into;

    /// Run the writer over a `#`-filled buffer, returning its answer and the
    /// buffer afterwards.
    fn run(value: i64, index: i64, len: usize) -> (Option<i64>, Vec<u8>) {
        let mut buf = vec![b'#'; len];
        let r = decimal_digits_latin1_into(value, index, &mut buf);
        (r, buf)
    }

    /// The bytes `DecimalDigits.uncheckedGetCharsLatin1` writes are the
    /// decimal rendering (`Integer.toString` / `Long.toString`), ending at
    /// `index`, and nothing else is touched; the answer is the new `charPos`.
    #[test]
    fn writes_exactly_the_decimal_rendering_ending_at_index() {
        let mut values: Vec<i64> = vec![
            0,
            1,
            -1,
            9,
            10,
            -10,
            99,
            100,
            -100,
            12_345,
            -98_765,
            i64::from(i32::MIN),
            i64::from(i32::MAX),
            i64::from(i32::MIN) + 1,
            i64::MIN,
            i64::MAX,
            i64::MIN + 1,
        ];
        let mut p: i64 = 1;
        for _ in 0..18 {
            values.extend([p - 1, p, p + 1, -(p - 1), -p, -(p + 1)]);
            p *= 10;
        }
        for v in values {
            let text = v.to_string();
            let n = text.len() as i64;
            let (r, buf) = run(v, 25, 30);
            assert_eq!(r, Some(25 - n), "charPos for {v}");
            let start = (25 - n) as usize;
            assert_eq!(&buf[start..25], text.as_bytes(), "digits of {v}");
            assert!(buf[..start].iter().all(|&b| b == b'#'), "before {v}");
            assert!(buf[25..].iter().all(|&b| b == b'#'), "after {v}");
        }
    }

    /// A dense sweep against Rust's own rendering, the int overload's range
    /// edges included.
    #[test]
    fn agrees_with_to_string_over_a_dense_sweep() {
        for v in (-200_000i64..200_000).chain([i64::from(i32::MIN), i64::from(i32::MAX)]) {
            let text = v.to_string();
            let (r, buf) = run(v, text.len() as i64, text.len());
            assert_eq!(r, Some(0), "{v}");
            assert_eq!(buf, text.as_bytes(), "{v}");
        }
    }

    /// Every range the JDK body would write outside the array is declined
    /// with nothing written: the caller then runs the real bytecode.
    #[test]
    fn an_out_of_range_write_is_declined_untouched() {
        for (value, index, len) in [
            (12_345i64, 4i64, 10usize), // digits would start at -1
            (-1, 1, 10),                // "-1" needs two bytes before index 1
            (7, 11, 10),                // index past the end
            (7, -1, 10),                // negative index
            (i64::MIN, 19, 19),         // 20 bytes into 19
            (0, 0, 0),                  // one digit into nothing
        ] {
            let (r, buf) = run(value, index, len);
            assert_eq!(r, None, "{value} at {index} in {len}");
            assert!(buf.iter().all(|&b| b == b'#'), "{value}: buffer written");
        }
        // The exact fit is served.
        assert_eq!(run(i64::MIN, 20, 20).0, Some(0));
        assert_eq!(run(-7, 2, 2), (Some(0), b"-7".to_vec()));
    }
}

/// Thin direct-call target for JIT `invokestatic
/// java/lang/ref/Reference.reachabilityFence(Object)` sites (registered into
/// `cratonvm_jit::REACHABILITY_FENCE_DIRECT_FN` by `build_helpers`).
///
/// Second on the same census — 3 200 000 calls, ~2 per `ByteBuffer` accessor —
/// and its registered body is `black_box(args.first()); Ok(None)`, i.e. it does
/// nothing but be opaque. Paying ~160 ns of generic native funnel for that is
/// pure loss.
///
/// **Deliberately still a CALL, not an elision.** HotSpot intrinsifies
/// `reachabilityFence` to no instructions at all, but it can afford to: its
/// compiler models the fence as a liveness constraint, so the referent stays in
/// the oop map without any code. This JIT has no such model, and the ONE thing
/// the method exists for is keeping the argument reachable across a region
/// where the compiler would otherwise consider it dead. Emitting nothing would
/// silently delete that guarantee, and the failure — an object collected while
/// a native still holds its address — is unreproducible and catastrophic.
/// Passing the reference to an opaque `extern "C"` function keeps it live in
/// the argument register and on the conservative scan, exactly as the
/// registered native did, while removing the funnel. ~160 ns becomes the cost
/// of a direct `CALL`.
///
/// Returns `0`, never the `i64::MIN` sentinel. The site is a `JitDirectCall`
/// with return type `V`, and that sequence still compares RAX against
/// `i64::MIN` after the CALL; a `()` return left the compare reading whatever
/// this body last put in RAX.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_reachability_fence_direct(
    _vm_ptr: i64,
    referent: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // The whole contract: be opaque about `referent` so nothing upstream may
    // conclude it is dead. `black_box` is what the registered native used.
    let _ = std::hint::black_box(referent);
    0
}

/// Synthetic call-site info for [`jit_object_get_class_direct`]'s decline edge.
///
/// A `const`, not a `static`: an immutable call descriptor is not process
/// state, and the one expression that takes its address in the helper below is
/// promoted to a single `'static` location, so the address the dispatcher sees
/// is stable. (`vm/tests/per_vm_state_statics_ratchet.rs` is frozen; this adds
/// no line to it.) `invoke_kind: 0` and `java/lang/Object`: the dispatcher
/// re-selects on the RECEIVER, exactly as the site would have without the bind.
const OBJECT_GET_CLASS_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "getClass",
    descriptor: "()Ljava/lang/Class;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `invokevirtual Object.getClass()` sites (JIT
/// round 17 wave 1, lane nio, NIO2-2; bound only under
/// `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT=1`).
///
/// # What it answers, and why that is the native's answer
///
/// `Object.getClass` is `ACC_NATIVE`, so there is no bytecode to shadow; the
/// registered body is `native_object_get_class`. Its common arm, for a plain
/// (non-array) object, is `get_class_mirror(display_id)` where `display_id` is
/// the stamped class unless `getclass_display_class_id` remaps it. This helper
/// answers ONLY when that remap is already memoised as "the stamped class"
/// (`getclass_display_is_cached_stamp`, the same per-thread memo the native
/// fills) and the class's mirror already exists in `class_mirrors` (the same
/// map `get_or_create_class_mirror` reads first). Those are exactly the
/// conditions of the SBR2 CALLER `LeafNativeKind::ObjectGetClass` arm, which
/// has served the MIC route since 2026-09-30; this helper removes the MIC round
/// trip in front of it.
///
/// Everything else -- an array (the native builds the array class's mirror),
/// a receiver whose class the memo has not classified yet or remaps, a class
/// with no mirror yet, a receiver that fails the heap screen -- declines to
/// `jit_invoke_dispatch`, which runs the registered native: the first call per
/// class and thread fills the memo, so the next one is served here.
///
/// A null receiver records the JEP 358 receiver action and returns the
/// sentinel, as every sibling helper and the generic dispatcher do, so the NPE
/// message is the one `invokevirtual` raises (HotSpot's `Cannot invoke
/// "Object.getClass()" because ... is null`).
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_object_get_class_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_object_get_class_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_object_get_class_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// nio20 L3 (NIO19-5): `class_id`'s mirror from `class_mirrors`, for a class
/// the CALLER has already shown to display as its stamp
/// (`getclass_display_is_cached_stamp`) -- and, while the read lock is still
/// held, that same mirror published as the class's compiled-`getClass()` word
/// (`ClassRealm::getclass_mirror_words`), so the next compiled `getClass()` of
/// this class answers with two loads instead of reaching here. Holding the
/// read lock across the publish is what keeps the word equal to the map: every
/// writer that moves or removes a mirror rewrites the word under the WRITE
/// lock. A no-op publish until a compile has allocated the table.
fn stamped_class_mirror_publishing_word(
    vm: &SharedVm,
    class_id: crate::classloading::ClassId,
) -> Option<ObjectRef> {
    let mirrors = vm.classes.class_mirrors.read();
    let mirror = mirrors.get(&class_id).copied()?;
    vm.classes.getclass_mirror_words.publish(class_id, mirror);
    Some(mirror)
}

/// nio21 wave 1, lane L5 (W4-L2-1, page
/// `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`): the mirror of the
/// one-dimensional primitive array class of `elem`, exactly as
/// `native_object_get_class`'s array arm answers a primitive array (its
/// reference-only arms -- lambda components, `strict_array_class_id`, the
/// per-loader synthetic mirror -- never apply): the unique class named `[I`
/// & co. (`class_id_by_name` is `find_unique_class_by_name`), then its entry in
/// `class_mirrors` (what `get_or_create_class_mirror` reads first). `None`
/// when either is absent -- the native then loads / creates it, and the next
/// call is served here. While the `class_mirrors` read lock is held the mirror
/// is published as primitive-array band `slot`'s word
/// (`GetClassMirrorWords::publish_prim_array`), so compiled `getClass()` of
/// such an array answers with one tag-byte screen and one load.
fn prim_array_class_mirror_publishing_word(
    vm: &SharedVm,
    elem: cratonvm_types::ArrayElementType,
    slot: usize,
) -> Option<ObjectRef> {
    use cratonvm_types::ArrayElementType as A;
    let name = match elem {
        A::Boolean => "[Z",
        A::Char => "[C",
        A::Float => "[F",
        A::Double => "[D",
        A::Byte => "[B",
        A::Short => "[S",
        A::Int => "[I",
        A::Long => "[J",
        A::Reference => return None,
    };
    let array_class_id = vm
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name(name)?;
    let mirrors = vm.classes.class_mirrors.read();
    let mirror = mirrors.get(&array_class_id).copied()?;
    vm.classes
        .getclass_mirror_words
        .publish_prim_array(slot, array_class_id, mirror);
    Some(mirror)
}

/// nio21 wave 2, lane L5 (L5W1-1, page
/// `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`): the mirror of the
/// array class over the reference component `component` (the header class id
/// of a reference array), when that answer is provably what
/// `native_object_get_class` returns for EVERY array over `component`, now
/// and later -- and, under the `class_mirrors` read lock, that mirror
/// published as the component's reference-array word.
///
/// # Why the conditions make it the native's answer in both modes
///
/// Let `name` be `[L<component>;` (or `[<component>` for an array component)
/// and `arr` the class filed under `(Bootstrap, name)` -- where
/// `synthesize_array_class_for_loader` files every array over a built-in
/// loader's component. Required: `component` is below the band (so no lambda
/// proxy: the native's lambda arm never applies), has a built-in defining
/// loader (`loader_id < 3`: the native's per-loader synthetic-mirror arm
/// never applies), `arr` is the UNIQUE class of that name, and `arr`'s
/// recorded component is `component`. Then the native's reference arms:
///
/// * `--compatible`: `strict_array_class_id` sees `Unique(arr)` with a
///   matching component and answers `None`; the by-name route
///   (`class_id_by_name` = `find_unique_class_by_name`) answers `arr`. Should
///   a user loader later define a same-named array, the name turns ambiguous
///   and `array_class_id_for_loader(name, <built-in>)` probes the bootstrap
///   key first: `arr` again.
/// * `--jdk-only`: before any defining loader exists, the by-name route
///   (`arr`); after, `array_class_id_for_loader`'s bootstrap probe (`arr`).
///
/// Bootstrap / platform / application classes are never unloaded, so `arr`
/// stays the answer; the word's mirror follows `arr`'s through the
/// collector's re-key / forget of `arr` (the owner map). Anything else -- a
/// user-loader component, a lambda component, an absent or ambiguous array
/// class, an absent mirror -- returns `None` and the call declines to the
/// native, as before.
fn ref_array_class_mirror_publishing_word(
    vm: &SharedVm,
    component: crate::classloading::ClassId,
) -> Option<ObjectRef> {
    if !cratonvm_jit::ir_lower::getclass_ref_array_words_enabled() {
        return None;
    }
    let cap = cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_CAPACITY;
    if usize::try_from(component.as_u32()).ok()? >= cap {
        return None;
    }
    let array_class_id = {
        let cm = vm.classes.class_manager.read();
        let loader = cm.get_loader_id(component)?;
        if loader.to_native_id() >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED
            || matches!(loader, cratonvm_types::ClassLoaderId::UserDefined(_))
        {
            return None;
        }
        let comp_name = std::sync::Arc::clone(&cm.get_class(component)?.name);
        let name = if comp_name.starts_with('[') {
            format!("[{comp_name}")
        } else {
            format!("[L{comp_name};")
        };
        let unique = cm.find_unique_class_by_name(&name)?;
        let boot =
            cm.loaded_class_under_exact_key(&name, cratonvm_types::ClassLoaderId::Bootstrap)?;
        if unique != boot {
            return None;
        }
        let recorded = cm
            .get_class(unique)?
            .array_info
            .as_ref()
            .map(|ai| ai.component_class_id)?;
        if recorded != component {
            return None;
        }
        unique
    };
    if usize::try_from(array_class_id.as_u32()).ok()? >= cap {
        return None;
    }
    let mirrors = vm.classes.class_mirrors.read();
    let mirror = mirrors.get(&array_class_id).copied()?;
    vm.classes
        .getclass_mirror_words
        .publish_ref_array(component, array_class_id, mirror);
    Some(mirror)
}

/// Body of [`jit_object_get_class_direct`], run under its panic guard.
unsafe fn jit_object_get_class_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = receiver as u64;
    // nio20 wave 4, lane L2: why this call was not answered here, for the
    // `CRATONVM_DBG_JITC` census below (a compiled prefix HIT never reaches
    // this helper, so every line here is a prefix miss or an unprefixed site).
    let mut decline = "not-an-object-address";
    let mut decline_cid: u32 = 0;
    // Shape screen before any header read, as `jit_buffer_session_direct` does.
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            let object = vm.mem.heap.load_and_forward(object);
            decline = "array";
            let kind = vm.mem.heap.kind_of(object);
            // nio21 wave 1, lane L5 (W4-L2-1): a one-dimensional PRIMITIVE
            // array is answered (and its word published) here; nio21 wave 2
            // L5: so is a REFERENCE array over a built-in loader's component
            // whose array class is unambiguous
            // (`ref_array_class_mirror_publishing_word`); any other reference
            // array still declines to the native (loader / strict-mode arms).
            if kind == cratonvm_types::ObjectKind::Array
                && cratonvm_jit::ir_lower::getclass_ref_array_words_enabled()
                && vm.mem.heap.element_type_of(object)
                    == cratonvm_types::ArrayElementType::Reference
            {
                let component = vm.mem.heap.class_id_of(object);
                decline_cid = component.as_u32();
                decline = "reference-array-not-publishable";
                if let Some(mirror) = ref_array_class_mirror_publishing_word(vm, component) {
                    if let Some((thread, _guard)) = jit_thread_mut() {
                        thread.native_pending_return = Some(mirror);
                    }
                    return mirror.as_ptr() as i64;
                }
            }
            if kind == cratonvm_types::ObjectKind::Array
                && cratonvm_jit::ir_lower::getclass_prim_array_words_enabled()
            {
                let elem = vm.mem.heap.element_type_of(object);
                if let Some(slot) = cratonvm_jit::ir_lower::getclass_prim_array_slot(elem) {
                    decline = "prim-array-class-or-mirror-absent";
                    if let Some(mirror) = prim_array_class_mirror_publishing_word(vm, elem, slot) {
                        if let Some((thread, _guard)) = jit_thread_mut() {
                            thread.native_pending_return = Some(mirror);
                        }
                        return mirror.as_ptr() as i64;
                    }
                } else if decline == "array" {
                    decline = "reference-array";
                }
            }
            if kind == cratonvm_types::ObjectKind::Object {
                let cid = vm.mem.heap.class_id_of(object);
                decline_cid = cid.as_u32();
                decline = "display-not-memoised-as-stamp";
                let vid = vm.vm_identity;
                if cratonvm_native_builtins::getclass_display_is_cached_stamp(vid, cid) {
                    decline = "no-mirror";
                    let mirror = stamped_class_mirror_publishing_word(vm, cid);
                    if let Some(mirror) = mirror {
                        if crate::runtime::env_cache::dbg_jitc() {
                            if let Some((served, declined)) =
                                vm.classes.getclass_mirror_words.note_helper_call(true)
                            {
                                eprintln!(
                                    "[cratonvm-jitc] getclass-helper served={served} \
                                     declined={declined} (last served class_id={} \
                                     word={:#x}; a published word that still reaches \
                                     here is a site with no prefix, or an id past the \
                                     table)",
                                    cid.as_u32(),
                                    vm.classes.getclass_mirror_words.get(cid),
                                );
                            }
                        }
                        // Object-return handoff root, as the leaf arm and
                        // `jit_thread_current_thread_direct` record it. The
                        // mirror is rooted by `class_mirrors` already.
                        if let Some((thread, _guard)) = jit_thread_mut() {
                            thread.native_pending_return = Some(mirror);
                        }
                        return mirror.as_ptr() as i64;
                    }
                }
            }
        }
    }
    if crate::runtime::env_cache::dbg_jitc() {
        if let Some((served, declined)) = vm.classes.getclass_mirror_words.note_helper_call(false) {
            eprintln!(
                "[cratonvm-jitc] getclass-helper served={served} declined={declined} \
                 (last decline: {decline} class_id={decline_cid})"
            );
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &OBJECT_GET_CLASS_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site info for [`jit_class_get_name_direct`]'s decline edge
/// (a `const`, as [`OBJECT_GET_CLASS_INFO`] and for its reason). `java/lang/Class`
/// is final, so the statically bound kind is the site's own call.
const CLASS_GET_NAME_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Class",
    method_name: "getName",
    descriptor: "()Ljava/lang/String;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `java/lang/Class.getName()` sites (JIT round
/// 22 wave 2, lane runtime, RUNTIME22-2; bound only under
/// `CRATONVM_JIT_CLASS_GET_NAME_DIRECT=1`).
///
/// `Class.getName()` is the VM's reviewed leaf `Intrinsic` (its JDK body
/// cannot run: this VM's mirrors hold the INTERNAL name in `Class.name`), and
/// on a warm class its whole answer is a memo hit
/// (`lang_class::native_class_get_name_leaf`). Compiled code reached that hit
/// through the dispatch funnel and the leaf door, ~150-500 ns against
/// HotSpot's field load. This helper asks the same two memo arms directly
/// (`lang_class::class_get_name_fast`) and answers the same, strongly held
/// `String`; every miss declines to `jit_invoke_dispatch` with the site's
/// triple, which runs the registered native as before (and fills the memo for
/// the next call). A null receiver records the JEP 358 receiver action and
/// returns the sentinel, as [`jit_object_get_class_direct`] does.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_class_get_name_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_class_get_name_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_class_get_name_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_class_get_name_direct`], run under its panic guard.
unsafe fn jit_class_get_name_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = receiver as u64;
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            let mirror = vm.mem.heap.load_and_forward(object);
            if let Some((thread, _guard)) = jit_thread_mut() {
                let answer = {
                    let ctx = crate::vm::NativeContextImpl {
                        shared: vm,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_class::class_get_name_fast(&ctx, mirror)
                };
                if let Some(name) = answer {
                    // Object-return handoff root, as the getClass helper
                    // records its mirror; the memo already holds `name`.
                    thread.native_pending_return = Some(name);
                    return name.as_ptr() as i64;
                }
            }
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &CLASS_GET_NAME_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site info for [`jit_buffer_session_direct`]'s decline edge.
///
/// `class_name` is the abstract base rather than a concrete buffer class, and
/// that is safe because the decline path re-dispatches on the RECEIVER: the
/// generic dispatcher resolves the same triple it would have resolved without
/// the bind, for whatever class the receiver actually is.
static BUFFER_SESSION_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/nio/Buffer",
    method_name: "session",
    descriptor: "()Ljdk/internal/foreign/MemorySessionImpl;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `java.nio.Buffer.session()` sites.
///
/// # What it replaces, and why that is exactly nothing
///
/// The registered shim's whole body is `Ok(Some(Value::Object(None)))` — a
/// constant null — and `--dump-native-registry` prices it at ONE crossing per
/// multi-byte heap accessor: 200 000 `session()` invocations for 200 000
/// `HeapByteBuffer.getInt(int)` calls, alongside the 200 000
/// `getIntUnaligned` crossings that do the actual read. Half the native
/// crossings on the hot accessor path were for a constant.
///
/// # `segment == null` is the bytecode's own answer, not a shadow's guess
///
/// Real `java.nio.Buffer.session()` is exactly `segment != null ?
/// segment.sessionImpl() : null` (`getfield segment; ifnull`). 2026-09-21: this
/// helper reads that same field — through
/// `resolve_field_slot_by_name_cached`, the identical cached resolver
/// `NativeContext::get_field_by_name` uses, so it can never disagree with a
/// real `getfield segment` on the same class — and when the field is null it
/// returns 0 directly. That is not standing in front of the real method the
/// way the old shim did; it IS the real method's own value for that receiver,
/// computed without a call. So it needs no class-name list, no registry
/// population and no §1.4 policy check: there is no bytecode being shadowed,
/// because this is what the bytecode computes.
///
/// When `segment` is non-null the real `sessionImpl()` call has to run — this
/// helper cannot answer it any faster than a call — so it declines
/// unconditionally to the generic dispatcher, which reaches the real bytecode.
///
/// # Why this exists (history)
///
/// Before this, the helper answered only for a class id
/// `buffer_session::class_is_served` said the OLD registered shim had already
/// run for — a real fact, but one `--jdk-only` strict policy could never
/// produce: a `Bridge`-kind native is never invoked when real bytecode is
/// authoritative (§1 rule 4), so the shim never ran, the table stayed empty,
/// and this helper declined on every single call under `--jdk-only` (census:
/// `served=0 declined=N`). The field read above needs no such census — it
/// answers from the receiver's own state, in every mode. The class-population
/// fallback below is kept only for the receiver shapes the field resolve
/// cannot reach (see its own comment); it is not expected to fire for any
/// loaded `Buffer` subclass.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_buffer_session_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_buffer_session_direct_body(vm_ptr, receiver);
    contain(
        "jit_buffer_session_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

// A layout proof for one native-owned segment/session pair. The current scope
// field is read on every hit; no mutable address or session value is cached.
// GC/free/close retire the epoch, metadata changes retire the layout epoch,
// and the VM identity prevents reuse between embedding instances.
#[derive(Clone, Copy)]
struct BufferStoredSessionProof {
    vm: usize,
    epoch: u64,
    layout_epoch: u64,
    segment: usize,
    session: usize,
    slot: usize,
}

thread_local! {
    static BUFFER_STORED_SESSION_PROOFS: std::cell::Cell<[BufferStoredSessionProof; 4]> =
        const { std::cell::Cell::new([BufferStoredSessionProof {
            vm: 0, epoch: 0, layout_epoch: 0, segment: 0, session: 0, slot: 0,
        }; 4]) };
    static BUFFER_STORED_SESSION_VICTIM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

unsafe fn buffer_stored_session(vm: &SharedVm, segment: ObjectRef) -> Option<ObjectRef> {
    buffer_stored_session_with(vm, segment, || {
        let (thread, _guard) = jit_thread_mut()?;
        let ctx = crate::vm::NativeContextImpl { shared: vm, thread };
        cratonvm_native_builtins::panama::jit_carrier_stored_session(&ctx, segment)
    })
}

unsafe fn buffer_stored_session_with(
    vm: &SharedVm,
    segment: ObjectRef,
    resolve: impl FnOnce() -> Option<(ObjectRef, usize)>,
) -> Option<ObjectRef> {
    let segment_address = segment.as_ptr() as usize;
    let epoch = cratonvm_native_builtins::ffm_fast::epoch();
    let layout_epoch = cratonvm_classloading::class_origin_epoch();
    let proofs = BUFFER_STORED_SESSION_PROOFS.with(std::cell::Cell::get);
    for proof in &proofs {
        if proof.vm == vm.vm_identity
            && proof.epoch == epoch
            && proof.layout_epoch == layout_epoch
            && proof.segment == segment_address
        {
            if let Value::Object(Some(session)) = vm.mem.heap.get_field(segment, proof.slot) {
                if session.as_ptr() as usize == proof.session {
                    return Some(session);
                }
            }
            // A rewritten scope must be validated by its owning native again.
            break;
        }
    }
    let (session, slot) = resolve()?;
    // The getter allocates nothing and cannot reach a safepoint. Even so, only
    // publish a proof whose epochs still match the checks that established it.
    if epoch == cratonvm_native_builtins::ffm_fast::epoch()
        && layout_epoch == cratonvm_classloading::class_origin_epoch()
    {
        let victim = BUFFER_STORED_SESSION_VICTIM.with(|c| {
            let victim = c.get();
            c.set((victim + 1) % proofs.len());
            victim
        });
        let mut updated = proofs;
        updated[victim] = BufferStoredSessionProof {
            vm: vm.vm_identity,
            epoch,
            layout_epoch,
            segment: segment_address,
            session: session.as_ptr() as usize,
            slot,
        };
        BUFFER_STORED_SESSION_PROOFS.with(|c| c.set(updated));
    }
    Some(session)
}

/// Body of [`jit_buffer_session_direct`], run under its panic guard.
unsafe fn jit_buffer_session_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // The `invokevirtual` this helper stands in for trapped on its
        // receiver: record the same JEP-358 action every sibling direct helper
        // and the generic dispatcher record, so `getMessage()` does not differ
        // by tier (round 11; this used the bare `set_jit_pending_npe()`).
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = receiver as u64;
    // Shape screen before any header read, as `dbb_direct_elem_addr` does:
    // this is the only thing between a fabricated argument and the class-id
    // probe below.
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        if let Some((class_id, _)) = direct_receiver_facts(vm, raw as usize) {
            let object = ObjectRef::from_raw(raw as usize as *mut u8);
            let class_id = ClassId::new(class_id);
            // The bytecode's own field, resolved the same way `getfield
            // segment` would be — see the doc comment above for why this
            // needs no policy check.
            if let Some(slot) =
                crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "segment")
            {
                let segment = vm.mem.heap.get_field(object, slot);
                if matches!(segment, Value::Object(None)) {
                    // `null`, exactly what `segment != null ? … : null`
                    // computes for a null segment — every caller on this path
                    // (`ScopedMemoryAccess.get*Unaligned`) already accepts it,
                    // skipping `checkValidStateRaw()` on a null session.
                    cratonvm_jit::BUFFER_SESSION_SERVED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return 0;
                }
                if let Value::Object(Some(segment)) = segment {
                    if let Some(session) = buffer_stored_session(vm, segment) {
                        cratonvm_jit::BUFFER_SESSION_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return session.as_ptr() as i64;
                    }
                }
                // A real segment's `sessionImpl()` remains authoritative.
                // Carrier shapes that require resolution also take the call.
            } else if cratonvm_native_builtins::buffer_session::class_is_served(class_id.as_u32())
                && !jit_direct_helper_refused(vm, &BUFFER_SESSION_GATE)
            {
                // Defensive fallback: a receiver whose class the field resolve
                // could not reach (should not happen for any loaded `Buffer`
                // subclass, since `segment` is declared on `Buffer` itself),
                // but which the OLD shim-population census already proved safe
                // to answer null for. See the type's doc comment for why this
                // path's own policy check (H12-1) still applies here and
                // nowhere else in this function.
                cratonvm_jit::BUFFER_SESSION_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return 0;
            }
        }
    }
    cratonvm_jit::BUFFER_SESSION_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &BUFFER_SESSION_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

// ============================================================================
// ScopedMemoryAccess unaligned thin direct helpers
// ============================================================================

static SCOPED_MEMORY_GET_BYTE_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getByteInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B",
    num_jit_args: 4,
    return_type: b'B',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};
static SCOPED_MEMORY_PUT_BYTE_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putByteInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V",
    num_jit_args: 5,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// A session verdict is published only by the checked native access. The same
// epoch and close handshake that retire segment verdicts retire this proof.
// The window remains open until after the memory operation, so a shared close
// cannot free the address between the memo lookup and the load/store.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_byte_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    contain(
        "jit_scoped_memory_get_byte_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
            let mut bytes = [0u8; 1];
            if window.admits_fast_path()
                && receiver != 0
                && !jit_direct_helper_refused(vm, &SCOPED_MEMORY_GET_BYTE_INTERNAL_GATE)
                && (session == 0
                    || cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
                && unsafe_accessor_capability_admits(vm, &UNSAFE_GET_BYTE_GATE)
                && raw_accessor_read_bytes(vm, obj, offset, &mut bytes)
            {
                cratonvm_jit::SCOPED_MEMORY_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return i64::from(bytes[0] as i8);
            }
            cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let args = [receiver, session, obj, offset];
            jit_invoke_dispatch(
                vm_ptr,
                &SCOPED_MEMORY_GET_BYTE_INTERNAL_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                4,
            )
        },
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_byte_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    value: i64,
) -> i64 {
    contain(
        "jit_scoped_memory_put_byte_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
            if window.admits_fast_path()
                && receiver != 0
                && !jit_direct_helper_refused(vm, &SCOPED_MEMORY_PUT_BYTE_INTERNAL_GATE)
                && (session == 0
                    || cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
                && unsafe_accessor_capability_admits(vm, &UNSAFE_PUT_BYTE_GATE)
                && raw_accessor_write_bytes(vm, obj, offset, &[value as u8])
            {
                cratonvm_jit::SCOPED_MEMORY_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return 0;
            }
            cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let args = [receiver, session, obj, offset, value];
            jit_invoke_dispatch(
                vm_ptr,
                &SCOPED_MEMORY_PUT_BYTE_INTERNAL_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                5,
            )
        },
    )
}

static SCOPED_MEMORY_GET_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getIntUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    num_jit_args: 5,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_GET_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getShortUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    num_jit_args: 5,
    return_type: b'S',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_GET_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getLongUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    num_jit_args: 5,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_GET_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getCharUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    num_jit_args: 5,
    return_type: b'C',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putIntUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putShortUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putLongUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putCharUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// `*Internal` twins -- see `ScopedMemoryOp`'s doc comment (jit/src/direct_helpers.rs)
// for why a decline must name the SAME method that was actually intercepted.

static SCOPED_MEMORY_GET_INT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getIntUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    num_jit_args: 5,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_GET_SHORT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getShortUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    num_jit_args: 5,
    return_type: b'S',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_GET_LONG_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getLongUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    num_jit_args: 5,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_GET_CHAR_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getCharUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    num_jit_args: 5,
    return_type: b'C',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_INT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putIntUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_SHORT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putShortUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_LONG_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putLongUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static SCOPED_MEMORY_PUT_CHAR_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putCharUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Shared fast arm of the eight `ScopedMemoryAccess.get*Unaligned{,Internal}`
/// thin helpers.
///
/// `gate` is the CALLER's, not this function's: the eight callers stand in
/// front of eight different registered rows, and a policy question asked about
/// one of them on behalf of another is the H7-1 defect one level down. It is
/// threaded rather than derived because this core has no way to tell which
/// accessor called it — the width is a slice length and the row is not.
unsafe fn scoped_memory_read_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    session: i64,
    obj: i64,
    offset: i64,
    out: &mut [u8],
) -> bool {
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    let unsafe_gate = match out.len() {
        2 => &UNSAFE_GET_SHORT_GATE,
        4 => &UNSAFE_GET_INT_GATE,
        8 => &UNSAFE_GET_LONG_GATE,
        _ => return false,
    };
    if !window.admits_fast_path()
        || jit_direct_helper_refused(vm, gate)
        || (session != 0
            && !cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
        || !unsafe_accessor_capability_admits(vm, unsafe_gate)
    {
        return false;
    }
    raw_accessor_read_bytes(vm, obj, offset, out)
}

/// Shared byte-copy core of [`scoped_memory_read_bytes`] and the plain
/// `Unsafe.get{Byte,Short,Int,Long}(Object,long)` thin helpers further below.
///
/// `Unsafe`'s own accessors take no `MemorySessionImpl` and have no
/// scoped-arena liveness to check -- real `Unsafe` does not check one either,
/// only `ScopedMemoryAccess` does, using its checked session verdict and
/// close window at the call site above. Declines (`false`) for anything but a primitive heap array
/// or an off-heap address, exactly as before this split, so a decline always
/// falls through to the generic dispatcher, never to a wrong answer.
unsafe fn raw_accessor_read_bytes(vm: &SharedVm, obj: i64, offset: i64, out: &mut [u8]) -> bool {
    if obj == 0 {
        let addr = offset;
        if addr <= 0 {
            return false;
        }
        if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr) {
            return cratonvm_native_builtins::unsafe_arena_copy_out(addr, out);
        } else if (addr as u64).checked_add(out.len() as u64).is_some() {
            std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len());
            return true;
        } else {
            return false;
        }
    }
    let raw = obj as u64;
    if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return false;
    }
    let Some(object) = vm.mem.heap.is_object_address(raw as usize) else {
        return false;
    };
    const ABASE: usize = 16;
    if offset < ABASE as i64 {
        return false;
    }
    let start = (offset as usize) - ABASE;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = vm.mem.heap.element_type_of_validated(object);
    if et == ArrayElementType::Reference {
        return false;
    }
    let width = cratonvm_types::element_byte_size(et);
    let Some(capacity) = vm.mem.heap.array_length(object).checked_mul(width) else {
        return false;
    };
    if start
        .checked_add(out.len())
        .map_or(true, |end| end > capacity)
    {
        return false;
    }
    let Some(base) = vm.mem.heap.array_data_ptr(object) else {
        return false;
    };
    std::ptr::copy_nonoverlapping(base.add(start), out.as_mut_ptr(), out.len());
    true
}

/// Write-side twin of [`scoped_memory_read_bytes`]; `gate` is the caller's for
/// the same reason.
unsafe fn scoped_memory_write_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    session: i64,
    obj: i64,
    offset: i64,
    data: &[u8],
) -> bool {
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    let unsafe_gate = match data.len() {
        2 => &UNSAFE_PUT_SHORT_GATE,
        4 => &UNSAFE_PUT_INT_GATE,
        8 => &UNSAFE_PUT_LONG_GATE,
        _ => return false,
    };
    if !window.admits_fast_path()
        || jit_direct_helper_refused(vm, gate)
        || (session != 0
            && !cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
        || !unsafe_accessor_capability_admits(vm, unsafe_gate)
    {
        return false;
    }
    raw_accessor_write_bytes(vm, obj, offset, data)
}

/// Write-side twin of [`raw_accessor_read_bytes`]; see its doc comment.
unsafe fn raw_accessor_write_bytes(vm: &SharedVm, obj: i64, offset: i64, data: &[u8]) -> bool {
    if obj == 0 {
        let addr = offset;
        if addr <= 0 {
            return false;
        }
        if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr) {
            return cratonvm_native_builtins::unsafe_arena_copy_in(addr, data);
        } else if (addr as u64).checked_add(data.len() as u64).is_some() {
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
            return true;
        } else {
            return false;
        }
    }
    let raw = obj as u64;
    if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return false;
    }
    let Some(object) = vm.mem.heap.is_object_address(raw as usize) else {
        return false;
    };
    const ABASE: usize = 16;
    if offset < ABASE as i64 {
        return false;
    }
    let start = (offset as usize) - ABASE;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = vm.mem.heap.element_type_of_validated(object);
    if et == ArrayElementType::Reference {
        return false;
    }
    let width = cratonvm_types::element_byte_size(et);
    let Some(capacity) = vm.mem.heap.array_length(object).checked_mul(width) else {
        return false;
    };
    if start
        .checked_add(data.len())
        .map_or(true, |end| end > capacity)
    {
        return false;
    }
    let Some(base) = vm.mem.heap.array_data_ptr(object) else {
        return false;
    };
    std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(start), data.len());
    true
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_int_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_int_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 4];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_INT_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i32::from_be_bytes(bytes)
        } else {
            i32::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_short_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_short_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_SHORT_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i16::from_be_bytes(bytes)
        } else {
            i16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_long_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_long_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 8];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_LONG_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i64::from_be_bytes(bytes)
        } else {
            i64::from_le_bytes(bytes)
        };
        return val;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_char_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_char_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_char_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_char_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_CHAR_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_CHAR_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.putIntUnaligned` fast path.
///
/// # The panic sentinel is `i64::MIN`, not `0` — r10 `help`
///
/// These four `put` shells were guarded with sentinel `0` while their own
/// bodies use `0` for **success** (the store was served) and `i64::MIN` for
/// failure, exactly like their `get` twins. The site is emitted with
/// `return_type = b'V'` (`ScopedMemoryOp::return_type`), and a `V` return takes
/// the plain arm of `emit_post_invoke_exception_check` — `CMP RAX, 1; JO bail`.
/// So a contained panic returning `0` told compiled code the store had
/// succeeded: the write was dropped, and the `InternalError` `OnPanic::Throw`
/// stashed was left on the pending channel for some unrelated later drain to
/// find. `i64::MIN` is the value that arm bails on, and is what every other
/// `Throw`-guarded direct helper in this file passes.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_int_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_int_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i32).to_be_bytes()
    } else {
        (val as i32).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_INT_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putShortUnaligned` fast path. Panic sentinel `i64::MIN`;
/// see [`jit_scoped_memory_put_int_direct`] for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_short_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_short_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i16).to_be_bytes()
    } else {
        (val as i16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_SHORT_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putLongUnaligned` fast path. Panic sentinel `i64::MIN`;
/// see [`jit_scoped_memory_put_int_direct`] for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_long_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_long_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        val.to_be_bytes()
    } else {
        val.to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_LONG_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putCharUnaligned` fast path. Panic sentinel `i64::MIN`;
/// see [`jit_scoped_memory_put_int_direct`] for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_char_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_char_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_char_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_char_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as u16).to_be_bytes()
    } else {
        (val as u16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_CHAR_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_CHAR_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.getIntUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_int_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getIntUnalignedInternal` by name, never to the public `getIntUnaligned`,
/// or a compiled `getIntUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_int_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_int_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_int_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_int_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 4];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_INT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i32::from_be_bytes(bytes)
        } else {
            i32::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_INT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.getShortUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_short_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getShortUnalignedInternal` by name, never to the public `getShortUnaligned`,
/// or a compiled `getShortUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_short_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_short_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_short_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_short_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_SHORT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i16::from_be_bytes(bytes)
        } else {
            i16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_SHORT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.getLongUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_long_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getLongUnalignedInternal` by name, never to the public `getLongUnaligned`,
/// or a compiled `getLongUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_long_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_long_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_long_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_long_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 8];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_LONG_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i64::from_be_bytes(bytes)
        } else {
            i64::from_le_bytes(bytes)
        };
        return val;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_LONG_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.getCharUnalignedInternal` fast path -- the private
/// twin of `jit_scoped_memory_get_char_direct`. See `ScopedMemoryOp`'s doc
/// comment (jit/src/direct_helpers.rs) for why this needs its own entry point
/// rather than sharing the public one's: a decline must dispatch to
/// `getCharUnalignedInternal` by name, never to the public `getCharUnaligned`,
/// or a compiled `getCharUnaligned` body whose own internal call site declines
/// recurses into itself forever.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_char_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_char_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_get_char_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_get_char_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_CHAR_INTERNAL_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_CHAR_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// `ScopedMemoryAccess.putIntUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_int_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_int_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_int_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_int_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_int_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i32).to_be_bytes()
    } else {
        (val as i32).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_INT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_INT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putShortUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_short_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_short_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_short_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_short_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_short_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as i16).to_be_bytes()
    } else {
        (val as i16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_SHORT_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_SHORT_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putLongUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_long_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_long_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_long_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_long_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_long_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val).to_be_bytes()
    } else {
        (val).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_LONG_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_LONG_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// `ScopedMemoryAccess.putCharUnalignedInternal` fast path -- see
/// `jit_scoped_memory_get_char_internal_direct`'s doc comment for why this
/// needs its own entry point. Panic sentinel `i64::MIN`; see
/// `jit_scoped_memory_put_int_direct` for why it is not `0`.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_char_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_put_char_internal_direct_body(
            vm_ptr, receiver, session, obj, offset, val, big_endian,
        )
    };
    contain(
        "jit_scoped_memory_put_char_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_scoped_memory_put_char_internal_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    val: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = if big_endian != 0 {
        (val as u16).to_be_bytes()
    } else {
        (val as u16).to_le_bytes()
    };
    if scoped_memory_write_bytes(
        vm,
        &SCOPED_MEMORY_PUT_CHAR_INTERNAL_GATE,
        session,
        obj,
        offset,
        &bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, val, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_PUT_CHAR_INTERNAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

// ============================================================================
// jdk/internal/misc/Unsafe plain scalar accessor thin direct helpers
// (heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918 "What is
// left" item 1)
// ============================================================================
//
// `Unsafe.get{Byte,Short,Int,Long}(Object,long)` / `put*` are genuinely
// `ACC_NATIVE`: the real JDK declares them `public native ... getInt(Object o,
// long offset);` with NO bytecode body. They are the floor underneath every
// `ScopedMemoryAccess.*Unaligned` call the helpers above decline (`session !=
// 0`, or under `--jdk-only`) and every `HeapByteBuffer`/netty
// `PlatformDependent` access that reaches `Unsafe` directly rather than
// through `ScopedMemoryAccess` -- measured at ~267 ns/call in BOTH modes
// (`UnsafeCrossingPerf`), i.e. already the generic-dispatch floor, not a
// `--jdk-only` regression. `raw_accessor_read_bytes`/`raw_accessor_write_bytes`
// (shared with the `ScopedMemoryAccess` family above) give the exact same
// answer a real `getfield`/array-element read would, computed without a call
// -- so, like `jit_buffer_session_direct`'s field read, this needs no
// `jit_direct_helper_refused`/§1.4 policy check: there is no bytecode being
// shadowed, because these methods have none. Declines to the generic
// dispatcher for anything the byte-copy core cannot serve (non-array objects,
// out-of-range offsets, reference arrays), which reaches the real registered
// native unchanged.
//
// No `bigEndian` argument on any of these -- the plain (non-`Unaligned`)
// `Unsafe` accessors always use the platform's native order, little-endian on
// every CratonVM host (see `unsafe_multibyte_get!`'s own doc comment in
// `native-builtins/src/unsafe_natives_ext.rs`).

static UNSAFE_GET_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getByte",
    descriptor: "(Ljava/lang/Object;J)B",
    num_jit_args: 3,
    return_type: b'B',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_GET_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getShort",
    descriptor: "(Ljava/lang/Object;J)S",
    num_jit_args: 3,
    return_type: b'S',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_GET_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getInt",
    descriptor: "(Ljava/lang/Object;J)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_GET_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getLong",
    descriptor: "(Ljava/lang/Object;J)J",
    num_jit_args: 3,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_GET_REFERENCE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getReference",
    descriptor: "(Ljava/lang/Object;J)Ljava/lang/Object;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_GET_REFERENCE_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getReferenceVolatile",
    descriptor: "(Ljava/lang/Object;J)Ljava/lang/Object;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_PUT_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putByte",
    descriptor: "(Ljava/lang/Object;JB)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_PUT_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putShort",
    descriptor: "(Ljava/lang/Object;JS)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_PUT_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putInt",
    descriptor: "(Ljava/lang/Object;JI)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_PUT_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putLong",
    descriptor: "(Ljava/lang/Object;JJ)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// NIO19-2: the triples [`UNSAFE_GET_CHAR_GATE`] / [`UNSAFE_PUT_CHAR_GATE`]
/// police. No thin helper stands in front of `getChar`/`putChar`; the gates
/// exist so [`unsafe_inline_admitted_mask`] can ask the same per-call policy
/// question about them that it asks about the other eight accessors before
/// the optimizing tier answers them inline. `const`, not `static`, so the
/// frozen `per_vm_state_statics_ratchet` count does not move (the gate's
/// `&INFO` is a promoted constant, as `OBJECT_GET_CLASS_INFO`'s is).
const UNSAFE_GET_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getChar",
    descriptor: "(Ljava/lang/Object;J)C",
    num_jit_args: 3,
    return_type: b'C',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_PUT_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putChar",
    descriptor: "(Ljava/lang/Object;JC)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// [`raw_accessor_read_bytes`], additionally policed by `gate`'s capability
/// — the read half of what `scoped_memory_read_bytes` does for its own
/// family, minus the scoped-arena liveness check that family alone needs.
unsafe fn unsafe_accessor_read_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    obj: i64,
    offset: i64,
    out: &mut [u8],
) -> bool {
    if jit_direct_helper_refused(vm, gate) || !unsafe_accessor_capability_admits(vm, gate) {
        return false;
    }
    raw_accessor_read_bytes(vm, obj, offset, out)
}

/// The funnel's own capability check for `gate`'s classification, run per
/// served call: under `Permissive` it is the per-thread first-use memo plus
/// one record, so the audit a served call bypasses the funnel for still sees
/// the use. `direct_helper_refusal` has already refused every other mode.
fn unsafe_accessor_capability_admits(vm: &SharedVm, gate: &'static DirectHelperGate) -> bool {
    match gate.capability {
        Some(kind) => crate::vm::check_native_dispatch_capability_for_slot(vm, kind, None).is_ok(),
        None => true,
    }
}

/// Write-side twin of [`unsafe_accessor_read_bytes`].
unsafe fn unsafe_accessor_write_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    obj: i64,
    offset: i64,
    data: &[u8],
) -> bool {
    if jit_direct_helper_refused(vm, gate) || !unsafe_accessor_capability_admits(vm, gate) {
        return false;
    }
    raw_accessor_write_bytes(vm, obj, offset, data)
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_byte_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_get_byte_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 1];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_BYTE_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(bytes[0] as i8);
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_short_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_short_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_get_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_SHORT_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(i16::from_le_bytes(bytes));
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_int_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_int_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_get_int_direct_body(vm_ptr: i64, receiver: i64, obj: i64, offset: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 4];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_INT_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(i32::from_le_bytes(bytes));
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_long_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_long_direct_body(vm_ptr, receiver, obj, offset);
    contain(
        "jit_unsafe_get_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_get_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 8];
    if unsafe_accessor_read_bytes(vm, &UNSAFE_GET_LONG_GATE, obj, offset, &mut bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from_le_bytes(bytes);
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_GET_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// Element `offset` of the REFERENCE ARRAY at `obj`, for the two reference
/// `Unsafe` read helpers (SBR round 2, wave 8, lane `tlocal`).
///
/// `Some(Some(object))` is the element, `Some(None)` a null element, `None`
/// declines (a null or unvalidatable base, a non-array, a primitive or
/// non-reference array, an offset that is not an element-aligned in-bounds byte
/// offset, a slot that does not hold a reference): the registered native then
/// answers, which is also the only thing that knows instance-field, static and
/// synthetic offsets.
///
/// The index decode is `unsafe_array_index_from_offset`'s byte-offset arm, spelled
/// out: `ABASE` 16, element scale 8 for `Reference`. The legacy "offset below
/// ABASE is already an index" arm is NOT served here (it exists for synthetic
/// callers; a real `ConcurrentHashMap.tabAt` offset is `ABASE + (i << 3)`).
///
/// Allocates nothing and cannot reach a safepoint, so the returned reference needs
/// only the object-return handoff root the callers set.
unsafe fn unsafe_reference_array_read(
    vm: &SharedVm,
    obj: i64,
    offset: i64,
) -> Option<Option<ObjectRef>> {
    let raw = obj as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let array = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(array) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    if vm.mem.heap.array_element_type(array) != Some(cratonvm_types::ArrayElementType::Reference) {
        return None;
    }
    const ABASE: i64 = 16;
    if offset < ABASE || (offset - ABASE) % 8 != 0 {
        return None;
    }
    let index = ((offset - ABASE) / 8) as usize;
    if index >= vm.mem.heap.array_length(array) {
        return None;
    }
    match vm.mem.heap.get_array_element(array, index) {
        Ok(Value::Object(element)) => Some(element),
        _ => None,
    }
}

/// Shared body of the two reference `Unsafe` read helpers.
unsafe fn jit_unsafe_get_reference_common(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    gate: &'static DirectHelperGate,
    info: &'static JitInvokeInfo,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, gate) && unsafe_accessor_capability_admits(vm, gate) {
        if let Some(read) = unsafe_reference_array_read(vm, obj, offset) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return match read {
                    Some(object) => {
                        // Object-return handoff root, same contract as every other
                        // thin helper that hands a reference to compiled code.
                        thread.native_pending_return = Some(object);
                        object.as_ptr() as i64
                    }
                    None => 0,
                };
            }
        }
        // JIT round 21 wave 1, lane updaters (UPDATERS21-1, opt-in): a field
        // base publishes its reference cell for the compiled get / put arm,
        // then takes the call exactly as before (this helper serves arrays).
        // Only when a compile also armed the reference get / put arm, which
        // is the one reader of the cell (cvm-r21w1 A/B: without it this cost
        // every rfu-get call a field lookup).
        if vm.jit.unsafe_inline.ref_volatile_publish_armed() {
            if let Some((target, slot)) = unsafe_volatile_field_target(vm, obj, offset) {
                crate::vm::vm_exec::note_unsafe_volatile_field_cell(vm, target, slot, b'L');
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_reference_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || {
        jit_unsafe_get_reference_common(
            vm_ptr,
            receiver,
            obj,
            offset,
            &UNSAFE_GET_REFERENCE_GATE,
            &UNSAFE_GET_REFERENCE_INFO,
        )
    };
    let bits = contain(
        "jit_unsafe_get_reference_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_reference_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || {
        jit_unsafe_get_reference_common(
            vm_ptr,
            receiver,
            obj,
            offset,
            &UNSAFE_GET_REFERENCE_VOLATILE_GATE,
            &UNSAFE_GET_REFERENCE_VOLATILE_INFO,
        )
    };
    let bits = contain(
        "jit_unsafe_get_reference_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Panic sentinel `i64::MIN`, not `0`: `0` is this family's SUCCESS value
/// (`return_type = b'V'` takes the plain arm of
/// `emit_post_invoke_exception_check`), same convention as
/// `jit_scoped_memory_put_int_direct`.
pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_byte_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_put_byte_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = [val as u8];
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_BYTE_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_short_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_short_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_put_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = (val as i16).to_le_bytes();
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_SHORT_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_int_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_int_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_put_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = (val as i32).to_le_bytes();
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_INT_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_long_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_long_direct_body(vm_ptr, receiver, obj, offset, val);
    contain(
        "jit_unsafe_put_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_put_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let bytes = val.to_le_bytes();
    if unsafe_accessor_write_bytes(vm, &UNSAFE_PUT_LONG_GATE, obj, offset, &bytes) {
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_PUT_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

static UNSAFE_CAS_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "compareAndSetInt",
    descriptor: "(Ljava/lang/Object;JII)Z",
    num_jit_args: 5,
    return_type: b'Z',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static UNSAFE_CAS_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "compareAndSetLong",
    descriptor: "(Ljava/lang/Object;JJJ)Z",
    num_jit_args: 5,
    return_type: b'Z',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// ---------------------------------------------------------------------------
// nio21 wave 3, lane L4: the `ACC_NATIVE` triples the `HttpClient` census
// (`Nio20HttpHot -Dnio20.rows=hc`, `--jdk-only` top 60) found compiled code
// dispatching generically (page
// `r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md`).
//
// Every fast arm below answers through the SAME `NativeContext` accessor the
// registered native reaches for the same input (`get_field_volatile`,
// `set_field_volatile`, `set_array_element`, `compare_and_swap_field_shared`,
// `get_field`), and declines every other shape -- a null or array base where
// the native would take another arm, a synthetic / arena / sentinel offset, a
// value word that is not a heap reference -- to the registered native through
// `jit_invoke_dispatch`. So memory ordering and GC barriers are the native's
// by construction: a volatile store is the accessor's SeqCst store, a
// reference store fires the collector's SATB pre-barrier
// (`publish_overwritten_field` on the Generational heap, inside the accessor on
// G1/ZGC) and card barrier, and a reference CAS is the VM's hardware
// `LOCK CMPXCHG` with SATB on `expected`. A reference-array volatile store adds
// one `MFENCE` after the accessor's store (the native's element store is a
// plain one; HotSpot's is a volatile one).
//
// The descriptors are `const`, not `static`, so the frozen
// `per_vm_state_statics_ratchet` count does not move (as `UNSAFE_GET_CHAR_INFO`).
// ---------------------------------------------------------------------------

const UNSAFE_GET_INT_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getIntVolatile",
    descriptor: "(Ljava/lang/Object;J)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_GET_LONG_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "getLongVolatile",
    descriptor: "(Ljava/lang/Object;J)J",
    num_jit_args: 3,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_PUT_INT_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putIntVolatile",
    descriptor: "(Ljava/lang/Object;JI)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_PUT_LONG_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putLongVolatile",
    descriptor: "(Ljava/lang/Object;JJ)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_PUT_REFERENCE_VOLATILE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "putReferenceVolatile",
    descriptor: "(Ljava/lang/Object;JLjava/lang/Object;)V",
    num_jit_args: 4,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_CAS_REFERENCE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "compareAndSetReference",
    descriptor: "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    num_jit_args: 5,
    return_type: b'Z',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const UNSAFE_FULL_FENCE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/Unsafe",
    method_name: "fullFence",
    descriptor: "()V",
    num_jit_args: 1,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// `refersTo0` is `private` on both classes, so the decline edge dispatches it
/// as `invokespecial` (the `ThreadLocal` private bodies' convention below).
const REFERENCE_REFERS_TO0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ref/Reference",
    method_name: "refersTo0",
    descriptor: "(Ljava/lang/Object;)Z",
    num_jit_args: 2,
    return_type: b'Z',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const PHANTOM_REFERENCE_REFERS_TO0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ref/PhantomReference",
    method_name: "refersTo0",
    descriptor: "(Ljava/lang/Object;)Z",
    num_jit_args: 2,
    return_type: b'Z',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

const THREAD_CURRENT_CARRIER_THREAD_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Thread",
    method_name: "currentCarrierThread",
    descriptor: "()Ljava/lang/Thread;",
    num_jit_args: 0,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The instance-field arm of the volatile `Unsafe` natives
/// (`native_unsafe_{get,put}_{int,long}_volatile`,
/// `native_unsafe_put_object_volatile`, `native_unsafe_cas_object`): a
/// non-null heap object that is not an array, at an offset that can only be a
/// field slot. `0..1<<16` excludes the synthetic side-store band
/// (`0x2000_0000..`), the `Buffer.address` sentinel and every arena handle,
/// which the native answers from other arms; `unsafe_arena_volatile_load` and
/// `unsafe_wide_raw_*` need a null base or an array, so neither precedes this
/// arm for such an input. The object is returned forwarded.
///
/// # SAFETY
///
/// `obj` is a raw reference a compiled frame is holding (validated here).
unsafe fn unsafe_volatile_field_target(
    vm: &SharedVm,
    obj: i64,
    offset: i64,
) -> Option<(ObjectRef, usize)> {
    if !(0..(1 << 16)).contains(&offset) {
        return None;
    }
    let raw = obj as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    let object = vm.mem.heap.load_and_forward(object);
    if vm.mem.heap.kind_of(object) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    // JIT round 20 wave 8c (lane huntDisp): under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` the offset is `16 + slot * 8 + k`
    // (`native-builtins` `unsafe_decode_field_offset`); an offset below the
    // base or inside a field (`k != 0`) declines to the native, which serves
    // the field's bytes.
    if vm.classes.unsafe_byte_field_offsets {
        // Cast: `offset` is in `0..1 << 16` (checked above).
        let offset = offset as usize;
        if offset < 16 || (offset - 16) & 7 != 0 {
            return None;
        }
        return Some((object, (offset - 16) >> 3));
    }
    Some((object, offset as usize))
}

/// The reference-ARRAY arm of `native_unsafe_put_object_volatile` /
/// `native_unsafe_cas_object`: `unsafe_checked_array_index`'s byte-offset
/// decode (`ABASE` 16, scale 8 for `Reference`), served only for an aligned,
/// in-bounds element of a reference array below the synthetic band. The legacy
/// "offset below ABASE is an index" arm is not served (as
/// [`unsafe_reference_array_read`]). Forwarded.
///
/// # SAFETY
///
/// As [`unsafe_volatile_field_target`].
unsafe fn unsafe_reference_array_slot(
    vm: &SharedVm,
    obj: i64,
    offset: i64,
) -> Option<(ObjectRef, usize)> {
    const ABASE: i64 = 16;
    if offset < ABASE || offset >= 0x2000_0000 || (offset - ABASE) % 8 != 0 {
        return None;
    }
    let raw = obj as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let array = vm.mem.heap.is_object_address(raw as usize)?;
    let array = vm.mem.heap.load_and_forward(array);
    if vm.mem.heap.kind_of(array) != cratonvm_types::ObjectKind::Array
        || vm.mem.heap.array_element_type(array)
            != Some(cratonvm_types::ArrayElementType::Reference)
    {
        return None;
    }
    let index = ((offset - ABASE) / 8) as usize;
    if index >= vm.mem.heap.array_length(array) {
        return None;
    }
    Some((array, index))
}

/// A reference operand word as the `Value` the funnel would have handed the
/// native: `0` is null, anything else must be a heap object (forwarded, as the
/// funnel's argument barrier forwards it); `None` declines.
///
/// # SAFETY
///
/// `raw` is a word a compiled frame passed as a reference (validated here).
unsafe fn unsafe_reference_operand(vm: &SharedVm, raw: i64) -> Option<Value> {
    if raw == 0 {
        return Some(Value::Object(None));
    }
    let bits = raw as u64;
    if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(bits as usize)?;
    Some(Value::Object(Some(vm.mem.heap.load_and_forward(object))))
}

/// The receiver null check every `Unsafe` thin helper makes first.
fn unsafe_null_receiver() -> i64 {
    set_jit_pending_npe_action(
        crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
    );
    i64::MIN
}

/// `getIntVolatile` (`wide == false`) / `getLongVolatile` (`wide == true`).
unsafe fn jit_unsafe_get_volatile_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    wide: bool,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let (gate, info): (&'static DirectHelperGate, &'static JitInvokeInfo) = if wide {
        (
            &UNSAFE_GET_LONG_VOLATILE_GATE,
            &UNSAFE_GET_LONG_VOLATILE_INFO,
        )
    } else {
        (&UNSAFE_GET_INT_VOLATILE_GATE, &UNSAFE_GET_INT_VOLATILE_INFO)
    };
    if !jit_direct_helper_refused(vm, gate) && unsafe_accessor_capability_admits(vm, gate) {
        if let Some((target, slot)) = unsafe_volatile_field_target(vm, obj, offset) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // A read is idempotent, so a value of another kind (the funnel
                // would coerce it) declines and the native re-reads it.
                match (wide, ctx.get_field_volatile(target, slot)) {
                    (true, Value::Long(v)) => {
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // JIT round 21 wave 1, lane updaters (opt-in).
                        crate::vm::vm_exec::note_unsafe_volatile_field_cell(
                            vm, target, slot, b'J',
                        );
                        return v;
                    }
                    (false, Value::Int(v)) => {
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // JIT round 21 wave 1, lane updaters (opt-in).
                        crate::vm::vm_exec::note_unsafe_volatile_field_cell(
                            vm, target, slot, b'I',
                        );
                        return i64::from(v);
                    }
                    _ => {}
                }
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_int_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_volatile_body(vm_ptr, receiver, obj, offset, false);
    contain(
        "jit_unsafe_get_int_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_get_long_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    let f = || jit_unsafe_get_volatile_body(vm_ptr, receiver, obj, offset, true);
    contain(
        "jit_unsafe_get_long_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `putIntVolatile` (`kind == b'I'`), `putLongVolatile` (`b'J'`) and
/// `putReferenceVolatile` (`b'L'`). Returns `0` served, else the dispatcher's
/// word (`i64::MIN` exactly when the native threw).
unsafe fn jit_unsafe_put_volatile_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
    kind: u8,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let (gate, info): (&'static DirectHelperGate, &'static JitInvokeInfo) = match kind {
        b'I' => (&UNSAFE_PUT_INT_VOLATILE_GATE, &UNSAFE_PUT_INT_VOLATILE_INFO),
        b'J' => (
            &UNSAFE_PUT_LONG_VOLATILE_GATE,
            &UNSAFE_PUT_LONG_VOLATILE_INFO,
        ),
        _ => (
            &UNSAFE_PUT_REFERENCE_VOLATILE_GATE,
            &UNSAFE_PUT_REFERENCE_VOLATILE_INFO,
        ),
    };
    if !jit_direct_helper_refused(vm, gate) && unsafe_accessor_capability_admits(vm, gate) {
        // An `int` operand arrives in the low 32 bits of its word.
        let value = match kind {
            b'I' => Some(Value::Int(val as i32)),
            b'J' => Some(Value::Long(val)),
            _ => unsafe_reference_operand(vm, val),
        };
        if let Some(value) = value {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // JIT round 21 (lane unsafe): only into a field of the
                // store's own kind; a `putIntVolatile` into a `long` / `float`
                // / `double` field (HotSpot: its low four bytes) and a
                // `putLongVolatile` into a `double` declines to the native,
                // whose lane store keeps the field's other bytes and kind.
                // (A reference store reads nothing here.)
                let same_kind = |target: ObjectRef, slot: usize| {
                    kind == b'L'
                        || !matches!(
                            (kind, ctx.get_field_volatile(target, slot)),
                            (b'I', Value::Long(_) | Value::Float(_) | Value::Double(_))
                                | (b'J', Value::Double(_))
                        )
                };
                if let Some((target, slot)) = unsafe_volatile_field_target(vm, obj, offset)
                    .filter(|&(target, slot)| same_kind(target, slot))
                {
                    // The native's own call: descriptor-aware SeqCst store,
                    // SATB publish of the overwritten reference, card barrier.
                    ctx.set_field_volatile(target, slot, value);
                    cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // JIT round 21 wave 1, lane updaters (opt-in): the int /
                    // long cell only; a reference put publishes nothing.
                    // UPDATERS21-1: the reference cell too.
                    crate::vm::vm_exec::note_unsafe_volatile_field_cell(vm, target, slot, kind);
                    return 0;
                }
                if kind == b'L' {
                    if let Some((array, index)) = unsafe_reference_array_slot(vm, obj, offset) {
                        // The native's element store (barriered), then the
                        // trailing StoreLoad a volatile store owes.
                        ctx.set_array_element(array, index, value);
                        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                        cratonvm_jit::UNSAFE_ACCESSOR_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return 0;
                    }
                }
            }
        }
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, val];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_int_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_volatile_body(vm_ptr, receiver, obj, offset, val, b'I');
    contain(
        "jit_unsafe_put_int_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_long_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_volatile_body(vm_ptr, receiver, obj, offset, val, b'J');
    contain(
        "jit_unsafe_put_long_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_put_reference_volatile_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    val: i64,
) -> i64 {
    let f = || jit_unsafe_put_volatile_body(vm_ptr, receiver, obj, offset, val, b'L');
    contain(
        "jit_unsafe_put_reference_volatile_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_reference_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || jit_unsafe_cas_reference_direct_body(vm_ptr, receiver, obj, offset, expected, new);
    contain(
        "jit_unsafe_cas_reference_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `compareAndSetReference`: an instance field or a reference-array element
/// through `compare_and_swap_field_shared` -- the body of the native's
/// `ctx.compare_and_swap_field` -- and nothing else. `CRATONVM_DBG_AQS_TRACE`
/// declines everything, as [`unsafe_cas_serve`] does, so the native's ledger
/// stays complete.
unsafe fn jit_unsafe_cas_reference_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if !cratonvm_native_builtins::util_concurrent_ext::aqs_trace_enabled()
        && !jit_direct_helper_refused(vm, &UNSAFE_CAS_REFERENCE_GATE)
        && unsafe_accessor_capability_admits(vm, &UNSAFE_CAS_REFERENCE_GATE)
    {
        if let (Some(exp), Some(nv)) = (
            unsafe_reference_operand(vm, expected),
            unsafe_reference_operand(vm, new),
        ) {
            let target = unsafe_volatile_field_target(vm, obj, offset)
                .or_else(|| unsafe_reference_array_slot(vm, obj, offset));
            if let Some((target, slot)) = target {
                let swapped =
                    crate::vm::vm_exec::compare_and_swap_field_shared(vm, target, slot, exp, nv);
                cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return i64::from(swapped);
            }
        }
    }
    cratonvm_jit::UNSAFE_CAS_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, expected, new];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_CAS_REFERENCE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_full_fence_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_unsafe_full_fence_direct_body(vm_ptr, receiver);
    contain(
        "jit_unsafe_full_fence_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `fullFence`: `std::sync::atomic::fence(SeqCst)`, which is `MFENCE` on
/// x86-64 -- what `native_unsafe_fence` does, minus the funnel.
unsafe fn jit_unsafe_full_fence_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &UNSAFE_FULL_FENCE_GATE)
        && unsafe_accessor_capability_admits(vm, &UNSAFE_FULL_FENCE_GATE)
    {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        cratonvm_jit::UNSAFE_ACCESSOR_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::UNSAFE_ACCESSOR_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_FULL_FENCE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// `Reference.refersTo0` / `PhantomReference.refersTo0`: exactly
/// `native_reference_refers_to` -- slot 0 read through `get_field_no_keepalive`
/// (NO keep-alive: `refersTo` must not resurrect the referent, so unlike
/// `jit_getfield`'s slot-0 SATB log this publishes nothing, under every
/// collector, and under an armed ZGC read barrier it does not mark either --
/// zt t23/zb8) and a pointer compare against the (forwarded) argument.
unsafe fn jit_refers_to0_body(vm_ptr: i64, receiver: i64, other: i64, phantom: bool) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        return unsafe_null_receiver();
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let (gate, info): (&'static DirectHelperGate, &'static JitInvokeInfo) = if phantom {
        (
            &PHANTOM_REFERENCE_REFERS_TO0_GATE,
            &PHANTOM_REFERENCE_REFERS_TO0_INFO,
        )
    } else {
        (&REFERENCE_REFERS_TO0_GATE, &REFERENCE_REFERS_TO0_INFO)
    };
    if !jit_direct_helper_refused(vm, gate) {
        if let (Some(Value::Object(Some(this))), Some(target)) = (
            unsafe_reference_operand(vm, receiver),
            unsafe_reference_operand(vm, other),
        ) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // zt t23/zb8: the non-keep-alive read (no mark under an
                // armed ZGC read barrier either).
                let referent = ctx.get_field_no_keepalive(this, 0);
                let result = match (referent, target) {
                    (Value::Object(Some(a)), Value::Object(Some(b))) => a.as_ptr() == b.as_ptr(),
                    (Value::Object(None), Value::Object(None)) => true,
                    _ => false,
                };
                note_funnel_bypass_hit();
                return i64::from(result);
            }
        }
    }
    let args = [receiver, other];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_reference_refers_to0_direct(
    vm_ptr: i64,
    receiver: i64,
    other: i64,
) -> i64 {
    let f = || jit_refers_to0_body(vm_ptr, receiver, other, false);
    contain(
        "jit_reference_refers_to0_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_phantom_reference_refers_to0_direct(
    vm_ptr: i64,
    receiver: i64,
    other: i64,
) -> i64 {
    let f = || jit_refers_to0_body(vm_ptr, receiver, other, true);
    contain(
        "jit_phantom_reference_refers_to0_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// `Thread.currentCarrierThread()`: the registered native is
/// `native_thread_current_thread`, the one `Thread.currentThread()` runs, so
/// this is [`jit_thread_current_thread_direct`]'s body with this triple's gate
/// and decline edge. (A mounted virtual thread is answered as the native
/// answers it; that the native then differs from HotSpot is the native's
/// page, not this helper's.)
pub(in crate::jit) unsafe extern "C" fn jit_thread_current_carrier_thread_direct(
    vm_ptr: i64,
) -> i64 {
    let f = || jit_thread_current_carrier_thread_direct_body(vm_ptr);
    let bits = contain(
        "jit_thread_current_carrier_thread_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

#[cfg(test)]
mod nio21_volatile_helper_tests {
    use super::*;
    use crate::config::VmConfig;

    /// The fast arms' target screens decline every input the registered
    /// native answers from ANOTHER arm (null base, synthetic band, a word that
    /// is not a heap object), and accept an instance field slot / an aligned
    /// in-bounds reference-array element.
    #[test]
    fn nio21_volatile_targets_decline_what_the_native_answers_elsewhere() {
        let shared = SharedVm::new(VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(cid) = cm.try_ensure_synthetic_class("cratonvm/test/Nio21VolatileHolder", 2)
            else {
                panic!("Compatible mode fabricates this fixture");
            };
            cid
        };
        let obj = shared.mem.heap.alloc_object(cid, 2);
        let raw = obj.as_ptr() as i64;
        let array = shared
            .mem
            .heap
            .alloc_array(cid, ArrayElementType::Reference, 4);
        let array_raw = array.as_ptr() as i64;
        // SAFETY: both words are live objects of this heap, or deliberately
        // not objects, which the screens are there to refuse.
        unsafe {
            assert!(
                unsafe_volatile_field_target(&shared, 0, 1).is_none(),
                "null base"
            );
            assert!(
                unsafe_volatile_field_target(&shared, raw, 0x2000_0000).is_none(),
                "synthetic"
            );
            assert!(unsafe_volatile_field_target(&shared, raw, -1).is_none());
            assert!(
                unsafe_volatile_field_target(&shared, raw + 4, 1).is_none(),
                "misaligned"
            );
            // Slot 1's Unsafe offset: `16 + 8` under the default
            // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (HotSpot byte offsets), the
            // slot index itself with `=0`.
            let slot1 = if shared.classes.unsafe_byte_field_offsets {
                24
            } else {
                1
            };
            let (target, slot) =
                unsafe_volatile_field_target(&shared, raw, slot1).expect("an instance field slot");
            assert_eq!(slot, 1);
            assert_eq!(target.as_ptr(), obj.as_ptr());
            assert!(
                unsafe_volatile_field_target(&shared, array_raw, 16).is_none(),
                "array base"
            );

            assert!(
                unsafe_reference_array_slot(&shared, raw, 16).is_none(),
                "not an array"
            );
            assert!(
                unsafe_reference_array_slot(&shared, array_raw, 8).is_none(),
                "below ABASE"
            );
            assert!(
                unsafe_reference_array_slot(&shared, array_raw, 20).is_none(),
                "unaligned"
            );
            assert!(
                unsafe_reference_array_slot(&shared, array_raw, 16 + 8 * 4).is_none(),
                "OOB"
            );
            let (a, index) =
                unsafe_reference_array_slot(&shared, array_raw, 16 + 8 * 3).expect("element 3");
            assert_eq!((a.as_ptr(), index), (array.as_ptr(), 3));

            assert_eq!(
                unsafe_reference_operand(&shared, 0),
                Some(Value::Object(None))
            );
            assert!(unsafe_reference_operand(&shared, 8).is_none());
            assert!(unsafe_reference_operand(&shared, raw + 1).is_none());
            assert_eq!(
                unsafe_reference_operand(&shared, raw),
                Some(Value::Object(Some(obj)))
            );
        }
    }
}

unsafe fn jit_thread_current_carrier_thread_direct_body(vm_ptr: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &THREAD_CURRENT_CARRIER_THREAD_GATE) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            // A carrier whose `currentThread()` was re-pointed at a mounted
            // virtual thread (`Thread.setCurrentThread`, the VM's
            // `jdk_continuation` module) answers its own thread: the native.
            if thread.jdk_continuation.carrier() == 0 {
                if let Some(obj) = thread.java_thread_obj {
                    thread.native_pending_return = Some(obj);
                    note_funnel_bypass_hit();
                    return obj.as_ptr() as i64;
                }
            }
        }
    }
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_CURRENT_CARRIER_THREAD_INFO as *const JitInvokeInfo as i64,
        0,
        0,
    )
}

/// The fast arm of the two `Unsafe.compareAndSet{Int,Long}` thin helpers:
/// `Some(swapped)` when this call is one the registered native would answer
/// with the hardware CAS, `None` to decline to it.
///
/// `native_unsafe_cas_int`/`_long` (`native-builtins/src/unsafe_natives_ext.rs`)
/// have five arms: a null base (static field, arena or side store), a synthetic
/// offset (side store), an array (locked element CAS), and an instance field,
/// which is `compare_and_swap_field(obj, offset, ..)` and, for a slot
/// `hw_atomic_addr` can address, one `LOCK CMPXCHG`. This serves ONLY that last
/// case, through [`crate::vm::vm_exec::hw_cas_primitive_field`] -- the same
/// function the native reaches, so an interpreted and a compiled CAS address
/// the same word in the same atomicity domain. Every other shape, including an
/// instance field whose LEGACY cell is not yet tagged with this width, is a
/// `None` and runs the native unchanged.
///
/// * A synthetic offset (`0x2000_0000..`) and the high `Buffer.address`
///   sentinel are far above any field index, so `hw_atomic_addr`'s own proof
///   -- the index must resolve to a DECLARED field of this width on the
///   receiver's class -- already excludes them. The explicit bound below only
///   saves the descriptor lookup.
/// * `CRATONVM_DBG_AQS_TRACE` declines everything, so the native's AQS ledger
///   stays complete while it is on.
/// * The capability gate is the funnel's own
///   (`check_native_dispatch_capability_for_slot`), via [`jit_direct_helper_refused`].
///
/// # SAFETY
///
/// `obj_raw` is a raw reference a compiled frame is holding.
unsafe fn unsafe_cas_serve(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    obj_raw: i64,
    offset: i64,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    if obj_raw == 0 || !(0..(1 << 16)).contains(&offset) {
        return None;
    }
    if cratonvm_native_builtins::util_concurrent_ext::aqs_trace_enabled()
        || jit_direct_helper_refused(vm, gate)
    {
        return None;
    }
    let obj = vm.mem.heap.is_object_address(obj_raw as usize)?;
    let fwd = vm.mem.heap.load_and_forward(obj);
    // JIT round 20 wave 8c (lane huntDisp): the slot of a byte field offset
    // (`unsafe_volatile_field_target`'s decode); inside a field declines to
    // the native.
    let index = if vm.classes.unsafe_byte_field_offsets {
        // Cast: `offset` is in `0..1 << 16` (checked above).
        let offset = offset as usize;
        if offset < 16 || (offset - 16) & 7 != 0 {
            return None;
        }
        (offset - 16) >> 3
    } else {
        offset as usize
    };
    crate::vm::vm_exec::hw_cas_primitive_field(vm, fwd, index, expected, new_val)
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_int_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || jit_unsafe_cas_int_direct_body(vm_ptr, receiver, obj, offset, expected, new);
    contain(
        "jit_unsafe_cas_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_cas_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // An `int` operand arrives in the low 32 bits of its word.
    if let Some(swapped) = unsafe_cas_serve(
        vm,
        &UNSAFE_CAS_INT_GATE,
        obj,
        offset,
        Value::Int(expected as i32),
        Value::Int(new as i32),
    ) {
        cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(swapped);
    }
    cratonvm_jit::UNSAFE_CAS_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, expected, new];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_CAS_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_long_direct(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || jit_unsafe_cas_long_direct_body(vm_ptr, receiver, obj, offset, expected, new);
    contain(
        "jit_unsafe_cas_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

unsafe fn jit_unsafe_cas_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(swapped) = unsafe_cas_serve(
        vm,
        &UNSAFE_CAS_LONG_GATE,
        obj,
        offset,
        Value::Long(expected),
        Value::Long(new),
    ) {
        cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return i64::from(swapped);
    }
    cratonvm_jit::UNSAFE_CAS_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, obj, offset, expected, new];
    jit_invoke_dispatch(
        vm_ptr,
        &UNSAFE_CAS_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, opt-in): the LEAF TWIN of
/// [`jit_unsafe_cas_int_direct`] (`wide == false`) and
/// [`jit_unsafe_cas_long_direct`] (`wide == true`): the helper's fast arm
/// (`unsafe_cas_serve`: one `LOCK CMPXCHG` on a proven instance field) and `-1`
/// -- never a `boolean` -- where the helper would raise the null receiver's
/// NPE or dispatch the registered native. The contract is
/// [`jit_preconditions_check_index_leaf`]'s: no allocation, no raise (a panic
/// is RECORDED and declines), no Java, no dispatch. A served CAS counts into
/// `UNSAFE_CAS_SERVED` as the helper's does; a decline counts nothing (the
/// helper it falls to counts it). The capability / `--jdk-only` gate is asked
/// again by the helper on a decline, so `direct_helper_jdk_only_*` may count a
/// declined call twice (as for the `checkIndex` twin).
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
#[allow(clippy::too_many_arguments)]
unsafe fn unsafe_cas_leaf(
    wide: bool,
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    let f = || {
        if receiver == 0 {
            return -1;
        }
        // SAFETY: as in `jit_unsafe_cas_int_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        let (gate, expected, new_val) = if wide {
            (
                &UNSAFE_CAS_LONG_GATE,
                Value::Long(expected),
                Value::Long(new),
            )
        } else {
            // An `int` operand arrives in the low 32 bits of its word.
            (
                &UNSAFE_CAS_INT_GATE,
                Value::Int(expected as i32),
                Value::Int(new as i32),
            )
        };
        match unsafe_cas_serve(vm, gate, obj, offset, expected, new_val) {
            Some(swapped) => {
                cratonvm_jit::UNSAFE_CAS_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                i64::from(swapped)
            }
            None => -1,
        }
    };
    contain("jit_unsafe_cas_leaf", OnPanic::RecordFor { vm_ptr }, -1, f)
}

/// [`unsafe_cas_leaf`] for `compareAndSetInt`.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_int_leaf(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    unsafe_cas_leaf(false, vm_ptr, receiver, obj, offset, expected, new)
}

/// [`unsafe_cas_leaf`] for `compareAndSetLong`.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_unsafe_cas_long_leaf(
    vm_ptr: i64,
    receiver: i64,
    obj: i64,
    offset: i64,
    expected: i64,
    new: i64,
) -> i64 {
    unsafe_cas_leaf(true, vm_ptr, receiver, obj, offset, expected, new)
}

/// Synthetic call-site info for [`jit_thread_current_thread_direct`]'s
/// cold arm (the first call on a thread whose mirror has not been built).
static THREAD_CURRENT_THREAD_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Thread",
    method_name: "currentThread",
    descriptor: "()Ljava/lang/Thread;",
    num_jit_args: 0,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Process-wide count of compiled-code native-funnel bypasses.
///
/// The counter exists for the same reason
/// `dispatch_static::INTRINSIC_HITS` does, and the source document says why
/// in as many words: *"the counter exists because two earlier attempts at
/// this shape were **inert**, and timings alone cannot tell 'never installed'
/// from 'installed but no faster'."* A JIT-side bypass has one extra way to
/// be inert that the interpreter one does not — `try_compile` may simply
/// never recognise the site — so a counter incremented in the emitted call's
/// own target is the only evidence that compiled code took this path.
///
/// Reported by `CRATONVM_INTRINSIC_STATS=1` alongside the interpreter count.
///
/// Was the process-global `JIT_FUNNEL_BYPASS_HITS`; now summed over the
/// per-thread [`JitCounterBlock`]s (r9-vmside), because every served
/// `VarHandle` direct call bumped it on top of its own counter.
///
/// Number of compiled-code native-funnel bypasses since process start.
pub fn jit_funnel_bypass_count() -> u64 {
    jit_counter_total(|b| {
        b.funnel_bypass_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// Thin direct-call target for JIT `invokestatic Thread.currentThread()`
/// sites (registered into `cratonvm_jit::THREAD_CURRENT_THREAD_DIRECT_FN` by
/// `build_helpers`; the recognition lives in `jit::try_compile`).
///
/// The JIT half of the bypass the interpreter got in
/// `dispatch_static.rs`'s `InterpIntrinsic::ThreadCurrentThread` arm. Once a
/// thread's `java_thread_obj` mirror exists, `currentThread()` is one field
/// read of a value that is **already a per-thread GC root**: there is no
/// argument to pin, nothing here allocates, collects or throws, and no Java
/// code runs. Everything `safe_native_call` does for it — the argument copy
/// and GC-forwarding barrier, `native_pin_roots`, the STW probe, the two
/// GC-pressure probes, two `thread_state::record_transition` calls, the
/// native ring, `catch_unwind`, the `memwatch`/`ec_watch` polls — is
/// bookkeeping for hazards this operation does not have.
///
/// What the compiled site paid instead was *worse* than the funnel: an
/// `invokestatic` whose callee is a registered native binds no compiled body,
/// so it fell through `jit_invoke_dispatch` to `vm_exec::invoke_or_native`,
/// which re-resolves the callee **by name** on every call and only then
/// enters the funnel.
///
/// The cold arm — a thread whose mirror has not been built yet, i.e. at most
/// once per thread — routes to `jit_invoke_dispatch` with the synthetic
/// call-site info above, which is byte-for-byte the route the site took
/// before this helper existed. That keeps `current_thread_object`'s
/// allocating slow path in exactly one place.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_current_thread_direct(vm_ptr: i64) -> i64 {
    let f = || jit_thread_current_thread_direct_body(vm_ptr);
    let bits = contain(
        "jit_thread_current_thread_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_thread_current_thread_direct`], run under its panic guard.
unsafe fn jit_thread_current_thread_direct_body(vm_ptr: i64) -> i64 {
    // Same Rust<->JIT boundary bookkeeping as every other direct helper: the
    // per-thread conservative-scan cache must be invalidated. No SATB flush —
    // the fast arm below cannot allocate or enter the GC barrier, and the
    // cold arm's `jit_invoke_dispatch` performs its own.
    crate::jit::conservative_roots::note_jit_boundary();
    // H12-1. `java/lang/Thread.currentThread()Ljava/lang/Thread;` is `bridge`,
    // and this is the helper the OSR door was MEASURED binding under
    // `--jdk-only` (298 000 calls, `OSR 1`, single-pass `0/7`). Declining here
    // takes the cold arm below — `jit_invoke_dispatch` with the synthetic
    // call-site info — which this function's doc already describes as
    // "byte-for-byte the route the site took before this helper existed", and
    // which is policy-checked.
    //
    // Checked up front rather than beside the `java_thread_obj` read: in strict
    // mode this helper must never serve, so every call IS a refusal, and the
    // only calls the counter over-reports relative to "fast arm suppressed" are
    // the ones with no thread mirror yet — at most one per thread.
    //
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the cold arm below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if !jit_direct_helper_refused(vm, &THREAD_CURRENT_THREAD_GATE) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(obj) = thread.java_thread_obj {
                // Mirror `safe_native_call`'s object-return handoff root,
                // exactly as `call_integer_native_raw` does. The mirror is
                // already rooted per-thread, so this is the diagnostic/contract
                // half rather than a liveness requirement; the wrapper drops it
                // again at the hand-off (handoff-w36b).
                thread.native_pending_return = Some(obj);
                note_funnel_bypass_hit();
                return obj.as_ptr() as i64;
            }
        }
    }
    // No mirror yet, no JIT thread context, or `JdkOnly` declined the fast arm:
    // the ordinary route, which is what this site did on every call before the
    // fast arm existed.
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_CURRENT_THREAD_INFO as *const JitInvokeInfo as i64,
        0,
        0,
    )
}

/// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`, opt-in): the LEAF TWIN of
/// [`jit_thread_current_thread_direct`]: its fast arm, and `0` (never a
/// thread mirror) where the helper would take its `jit_invoke_dispatch` cold
/// arm. See [`jit_preconditions_check_index_leaf`] for the contract.
///
/// The helper sets `native_pending_return` and its wrapper drops it again at
/// the hand-off (`native_return_handed_to_compiled_code`); the twin does
/// neither, which leaves the same state behind: the mirror is rooted
/// per-thread already, and the handoff root was never a liveness requirement.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_current_thread_leaf(vm_ptr: i64) -> i64 {
    let f = || {
        // SAFETY: as in `jit_thread_current_thread_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        if jit_direct_helper_refused(vm, &THREAD_CURRENT_THREAD_GATE) {
            return 0;
        }
        match jit_thread_mut() {
            Some((thread, _guard)) => match thread.java_thread_obj {
                Some(obj) => {
                    note_funnel_bypass_hit();
                    obj.as_ptr() as i64
                }
                None => 0,
            },
            None => 0,
        }
    };
    contain("jit_thread_current_thread_leaf", OnPanic::RecordFor { vm_ptr }, 0, f)
}

// ---------------------------------------------------------------------------
// SBR round 2, wave 8 (lane `tlocal`): `ThreadLocal.get()` thin direct call.
//
// Under `--jdk-only` `ThreadLocal.get()` is a reviewed `Intrinsic`
// (`native_tl_real_get`, `native-builtins/src/phases_early.rs`) that reads the
// REAL `Thread.threadLocals` table and runs the JDK's own `get(Thread)`
// bytecode for every case it does not answer. From compiled code that native
// crossing still costs a dispatch round trip; this helper answers the same hot
// case (the key found by reading the probe sequence, no stale entry met) with
// no dispatch at all and DECLINES everything else, to the same dispatch the
// site had before, which then runs the native (and through it the bytecode).
//
// The helper never decides anything the native would decide differently:
//   * `--compatible` and any mode where the real-field natives are not the
//     registered ones (`CRATONVM_TL_REAL_FAST=0`, `CRATONVM_TL_INTRINSIC=1`)
//     always decline (`thread_local_real_fast_active`);
//   * the receiver's class must be plain `ThreadLocal`, `InheritableThreadLocal`
//     or a subclass of either that neither overrides `get()` nor is a
//     `TerminatingThreadLocal` (the memo below, stamped with the class-set epoch);
//   * a missing map or table, a key not found, a cleared key on the probe path
//     (`expungeStaleEntry` is the bytecode's business) all decline.
// ---------------------------------------------------------------------------

/// The JDK's own private bodies, which the two helpers dispatch to DIRECTLY when
/// the receiver is a class the helper may answer for and the key is simply not
/// in the table (the first `get` of a thread, a `set` after `remove`, a cleared
/// key on the probe path): `ThreadLocal.get(Thread)` / `ThreadLocal.set(Thread,
/// Object)`. They have no registered native, so the dispatch runs the bytecode
/// without the round trip through the public method's reviewed `Intrinsic`
/// (which found nothing either and then invoked the same bytecode from inside a
/// native: measured, a `set` of a new key cost 2-3 us more that way).
static THREAD_LOCAL_GET_PRIVATE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "get",
    descriptor: "(Ljava/lang/Thread;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static THREAD_LOCAL_SET_PRIVATE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "set",
    descriptor: "(Ljava/lang/Thread;Ljava/lang/Object;)V",
    num_jit_args: 3,
    return_type: b'V',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// May the private `get(Thread)` / `set(Thread, Object)` bodies run for `this`
/// with no re-dispatch on its class? Only when no class between `this` and
/// `ThreadLocal` declares its own `get()` (and, for `set`, tracing is off, since
/// the public body then prints a stack trace); `TerminatingThreadLocal` keeps the
/// generic dispatch. Asked only on the miss path.
unsafe fn tl_miss_private_ok(vm: &SharedVm, this: ObjectRef, for_set: bool) -> bool {
    let Some(l) = tl_get_layout(vm) else {
        return false;
    };
    if for_set && l.trace {
        return false;
    }
    let cid = vm.mem.heap.class_id_of_validated(this);
    // The private `set(Thread, T)` is `ThreadLocal.set`'s body: a receiver
    // whose class overrides `set(Object)` must run its override instead.
    if for_set && !tl_set_receiver_ok(vm, &l, cid) {
        return false;
    }
    tl_get_receiver_kind(vm, &l, cid) != TL_MAP_BYTECODE
}

/// Synthetic call-site info for [`jit_thread_local_set_direct`]'s decline edge.
static THREAD_LOCAL_SET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "set",
    descriptor: "(Ljava/lang/Object;)V",
    num_jit_args: 2,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Synthetic call-site info for [`jit_thread_local_get_direct`]'s decline edge.
static THREAD_LOCAL_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/ThreadLocal",
    method_name: "get",
    descriptor: "()Ljava/lang/Object;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Which `Thread` field `ThreadLocal.getMap` reads for a receiver class.
const TL_MAP_PLAIN: u8 = 0;
const TL_MAP_INHERITABLE: u8 = 1;
const TL_MAP_BYTECODE: u8 = 2;

/// Class ids and slot indices the fast path reads, resolved once per VM per
/// thread. Never a process global: another VM in the process may lay these
/// classes out differently (`vm` is the key).
#[derive(Clone, Copy)]
struct TlGetLayout {
    vm: usize,
    tl: ClassId,
    itl: ClassId,
    term: ClassId,
    /// `ThreadLocal$ThreadLocalMap$Entry`: the raw-read arm
    /// ([`tl_load_ref_field`]) believes a table element only when its header
    /// names this class (nio21 wave 17 L2).
    entry: ClassId,
    hash_slot: usize,
    thread_tl_slot: usize,
    thread_itl_slot: usize,
    table_slot: usize,
    value_slot: usize,
    /// `jdk.traceVirtualThreadLocals` was `true` when this was resolved: `set` then
    /// runs the JDK body (it prints a stack trace for virtual threads).
    trace: bool,
}

thread_local! {
    static TL_GET_LAYOUT: Cell<Option<TlGetLayout>> = const { Cell::new(None) };
    /// `(class-set epoch, [(receiver class id, TL_MAP_* verdict)])`.
    static TL_GET_KIND_MEMO: std::cell::RefCell<(u64, Vec<(u32, u8)>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };
}

/// Resolve [`TlGetLayout`], or `None` while any of the classes is not loaded
/// (nothing is cached then, so the next call retries).
fn tl_get_layout(vm: &SharedVm) -> Option<TlGetLayout> {
    if let Some(l) = TL_GET_LAYOUT.with(Cell::get) {
        if l.vm == vm.vm_identity {
            return Some(l);
        }
    }
    let cm = vm.classes.class_manager.read();
    let tl = cm.get_loaded_class_id("java/lang/ThreadLocal")?;
    let itl = cm.get_loaded_class_id("java/lang/InheritableThreadLocal")?;
    let term = cm.get_loaded_class_id("jdk/internal/misc/TerminatingThreadLocal")?;
    let thread = cm.get_loaded_class_id("java/lang/Thread")?;
    let map = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap")?;
    let entry = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap$Entry")?;
    let store = &cm.class_store;
    let layout = TlGetLayout {
        vm: vm.vm_identity,
        tl,
        itl,
        term,
        entry,
        hash_slot: crate::vm::resolve_field_index_in_hierarchy(tl, "threadLocalHashCode", store)?,
        thread_tl_slot: crate::vm::resolve_field_index_in_hierarchy(thread, "threadLocals", store)?,
        thread_itl_slot: crate::vm::resolve_field_index_in_hierarchy(
            thread,
            "inheritableThreadLocals",
            store,
        )?,
        table_slot: crate::vm::resolve_field_index_in_hierarchy(map, "table", store)?,
        value_slot: crate::vm::resolve_field_index_in_hierarchy(entry, "value", store)?,
        trace: vm
            .system_properties
            .read()
            .get("jdk.traceVirtualThreadLocals")
            .is_some_and(|v| v.eq_ignore_ascii_case("true")),
    };
    drop(cm);
    TL_GET_LAYOUT.with(|c| c.set(Some(layout)));
    Some(layout)
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_EARLY_LAYOUT` (page
/// `jr21-tl3-threadlocal-subclass-get-takes-the-thin-helper-FIXED-20261014.md`, the
/// `R21TlsubRows` / `R21Tl3Rows` shape), **default ON**, `=0` kill: read per
/// compile, never cached.
fn tl_early_layout_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_THREAD_LOCAL_EARLY_LAYOUT")
}

/// The COMPILER's view of [`TlGetLayout`] (`jit_thread_local_ir_layout`):
/// the full layout when [`tl_get_layout`] has it (`true`), else -- under
/// [`tl_early_layout_enabled`] -- the same resolution with
/// `InheritableThreadLocal` and `jdk/internal/misc/TerminatingThreadLocal`
/// allowed to be unloaded (`false`; each missing one is class id `0`, which
/// names no class). Never cached: the helpers' per-thread layout must name
/// both, or their kind walk would answer `TL_MAP_PLAIN` for an
/// `InheritableThreadLocal` once it loads.
///
/// Why a compile may bake it: a class is loaded after its superclass, so while
/// either is unloaded no loaded class descends from it. Every class id the
/// expansion bakes (`ThreadLocal`, an admitted `SuppliedThreadLocal`, a
/// profiled subclass whose kind [`tl_ir_receiver_kind`] computed now) is
/// tested EXACTLY, and an object never changes class, so the verdict baked for
/// it stays true after either class loads. An `InheritableThreadLocal` site
/// keeps its call (layout word 15 is `0`). Before this, a method compiled
/// before the program's first `ThreadLocal.set` -- which loads
/// `TerminatingThreadLocal` -- refused every expansion for the life of its
/// code (`R21TlsubRows`, row `arith` first: ~150 ns a get in every arm).
fn tl_ir_layout(vm: &SharedVm) -> Option<(TlGetLayout, bool)> {
    if let Some(l) = tl_get_layout(vm) {
        return Some((l, true));
    }
    if !tl_early_layout_enabled() {
        return None;
    }
    let cm = vm.classes.class_manager.read();
    let tl = cm.get_loaded_class_id("java/lang/ThreadLocal")?;
    let none = ClassId::new(0);
    let itl = cm
        .get_loaded_class_id("java/lang/InheritableThreadLocal")
        .unwrap_or(none);
    let term = cm
        .get_loaded_class_id("jdk/internal/misc/TerminatingThreadLocal")
        .unwrap_or(none);
    let thread = cm.get_loaded_class_id("java/lang/Thread")?;
    let map = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap")?;
    let entry = cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap$Entry")?;
    let store = &cm.class_store;
    let layout = TlGetLayout {
        vm: vm.vm_identity,
        tl,
        itl,
        term,
        entry,
        hash_slot: crate::vm::resolve_field_index_in_hierarchy(tl, "threadLocalHashCode", store)?,
        thread_tl_slot: crate::vm::resolve_field_index_in_hierarchy(thread, "threadLocals", store)?,
        thread_itl_slot: crate::vm::resolve_field_index_in_hierarchy(
            thread,
            "inheritableThreadLocals",
            store,
        )?,
        table_slot: crate::vm::resolve_field_index_in_hierarchy(map, "table", store)?,
        value_slot: crate::vm::resolve_field_index_in_hierarchy(entry, "value", store)?,
        trace: vm
            .system_properties
            .read()
            .get("jdk.traceVirtualThreadLocals")
            .is_some_and(|v| v.eq_ignore_ascii_case("true")),
    };
    Some((layout, false))
}

/// [`tl_get_receiver_kind`] for a layout from [`tl_ir_layout`]: a partial one
/// (`complete == false`) walks the chain afresh, past the per-thread memo,
/// which the helpers fill under the full layout only.
fn tl_ir_receiver_kind(vm: &SharedVm, l: &TlGetLayout, complete: bool, cid: ClassId) -> u8 {
    if complete {
        tl_get_receiver_kind(vm, l, cid)
    } else if cid == l.tl {
        TL_MAP_PLAIN
    } else {
        tl_get_compute_kind(vm, l, cid)
    }
}

/// The superclass walk behind [`tl_get_receiver_kind`]: `getMap` for the class
/// chain of `cid`, `TL_MAP_BYTECODE` for anything the helper must not answer.
fn tl_get_compute_kind(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> u8 {
    let cm = vm.classes.class_manager.read();
    let mut c = cid;
    let mut inheritable = false;
    for _ in 0..32 {
        if c == l.term {
            return TL_MAP_BYTECODE;
        }
        let Some(class) = cm.class_store.get(c) else {
            return TL_MAP_BYTECODE;
        };
        // A subclass that declares its own `get()` is dispatched to by the
        // site's virtual call; the helper bound against `ThreadLocal.get` must
        // not answer for it. JIT round 21 (lane tlsub, wave 21; correctness,
        // unconditional): a BRIDGE `get()Object` counts too. javac emits one
        // exactly when the class overrides `get` with a narrower return type
        // (`public int[] get()` in a `ThreadLocal<int[]>` subclass: the real
        // body is `get()[I`, which this descriptor never names), so skipping
        // bridges served such a receiver the raw map value, past its override
        // (`R21TlsubRows tl-get-override`, `fact override-gets`). `ThreadLocal`
        // is public, so no visibility bridge forwards to its own `get`.
        if c != l.tl && class.find_method("get", "()Ljava/lang/Object;").is_some() {
            return TL_MAP_BYTECODE;
        }
        if c == l.itl {
            inheritable = true;
        }
        if c == l.tl {
            return if inheritable {
                TL_MAP_INHERITABLE
            } else {
                TL_MAP_PLAIN
            };
        }
        match class.superclass {
            Some(parent) => c = parent,
            None => return TL_MAP_BYTECODE,
        }
    }
    TL_MAP_BYTECODE
}

/// JIT round 21 (lane tlsub, TL321-4): does a class on `cid`'s superclass
/// chain below `ThreadLocal` declare its own `set(Object)` (a bridge too)? Such
/// a receiver's `set` is dispatched to that body, so the optimizing tier's
/// `set` expansion must not answer for it. A chain that does not reach
/// `ThreadLocal` (or an unknown class) answers `true`, the refusing answer.
/// Compile time only (`jit_thread_local_ir_layout`'s one-class query).
fn tl_class_declares_set(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> bool {
    let cm = vm.classes.class_manager.read();
    let mut c = cid;
    for _ in 0..32 {
        if c == l.tl {
            return false;
        }
        let Some(class) = cm.class_store.get(c) else {
            return true;
        };
        // A bridge counts (JIT round 21, lane tlsub, wave 21; correctness,
        // unconditional): `set(int[])` in a `ThreadLocal<int[]>` subclass is
        // reached through its bridge `set(Object)`, as `tl_get_compute_kind`'s
        // `get` override.
        if class.find_method("set", "(Ljava/lang/Object;)V").is_some() {
            return true;
        }
        match class.superclass {
            Some(parent) => c = parent,
            None => return true,
        }
    }
    true
}

/// The `Thread` field `ThreadLocal.getMap` reads for a receiver of class `cid`,
/// memoised per thread under the class-set epoch (an unload frees a `ClassId`
/// for reuse, so an entry is believed only under the epoch it was proven in).
fn tl_get_receiver_kind(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> u8 {
    if cid == l.tl {
        return TL_MAP_PLAIN;
    }
    if cid == l.itl {
        return TL_MAP_INHERITABLE;
    }
    tl_kind_memo(vm, cid.as_u32(), || tl_get_compute_kind(vm, l, cid))
}

/// JIT round 21 (lane tlsub, wave 21; correctness, unconditional): may the
/// `set` helper (`jit_thread_local_set_direct`, both its found-key store and
/// its private `set(Thread, T)` miss arm) answer for a receiver of `cid`? Only
/// when no class below `ThreadLocal` declares `set(Object)`
/// ([`tl_class_declares_set`], a bridge included): the site's virtual call
/// dispatches to such an override, which the helper used to skip whenever the
/// key was present (it screened only `get` overrides, through the kind).
/// `ThreadLocal` and `InheritableThreadLocal` declare no override. Memoised
/// beside the kinds under the bit-31-tagged key (class ids are dense indices,
/// far below `1 << 31`; an id with that bit set is refused).
fn tl_set_receiver_ok(vm: &SharedVm, l: &TlGetLayout, cid: ClassId) -> bool {
    if cid == l.tl || cid == l.itl {
        return true;
    }
    let raw = cid.as_u32();
    if raw & TL_SET_MEMO_TAG != 0 {
        return false;
    }
    tl_kind_memo(vm, raw | TL_SET_MEMO_TAG, || {
        u8::from(!tl_class_declares_set(vm, l, cid))
    }) == 1
}

/// The key bit [`tl_set_receiver_ok`]'s rows carry in `TL_GET_KIND_MEMO`.
const TL_SET_MEMO_TAG: u32 = 1 << 31;

/// `TL_GET_KIND_MEMO`'s lookup-or-compute for `key`, under the class-set epoch.
fn tl_kind_memo(vm: &SharedVm, key: u32, compute: impl FnOnce() -> u8) -> u8 {
    let epoch = typecheck_memo_epoch(vm);
    let hit = TL_GET_KIND_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.0 != epoch {
            m.1.clear();
            m.0 = epoch;
            return None;
        }
        m.1.iter().find(|(c, _)| *c == key).map(|(_, k)| *k)
    });
    if let Some(kind) = hit {
        return kind;
    }
    let kind = compute();
    if typecheck_memo_epoch(vm) == epoch {
        TL_GET_KIND_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            if m.0 == epoch {
                if m.1.len() >= 16 {
                    m.1.remove(0);
                }
                m.1.push((key, kind));
            }
        });
    }
    kind
}

/// The entry of the current thread's real table whose referent IS `this`, or
/// `None` (no map or table, key absent, a cleared key on the probe path, a
/// receiver class the helper must not answer for). Allocates nothing and cannot
/// reach a safepoint.
unsafe fn tl_find_entry(
    vm: &SharedVm,
    l: &TlGetLayout,
    mirror: ObjectRef,
    this: ObjectRef,
) -> Option<ObjectRef> {
    let heap = &vm.mem.heap;
    let cid = heap.class_id_of_validated(this);
    let field = match tl_get_receiver_kind(vm, l, cid) {
        TL_MAP_PLAIN => l.thread_tl_slot,
        TL_MAP_INHERITABLE => l.thread_itl_slot,
        _ => return None,
    };
    // nio21 wave 17 L2: every read below goes through the raw-displacement
    // readers (`CRATONVM_JIT_TL_RAW_FIELDS`, default ON), which fall back to
    // the generic accessor per read when the switch is off or the body is not
    // compact. A `None` from any of them declines, exactly like a miss.
    let raw = crate::runtime::env_cache::jit_tl_raw_fields();
    let map = tl_load_ref_field(heap, raw, mirror, field)??;
    let table = tl_load_ref_field(heap, raw, map, l.table_slot)??;
    // The element reads below index the body directly, so the table must
    // really be a reference array (the generic `get_array_element` checked
    // this per element; once per call is the same question).
    let table_header = &*(table.as_ptr() as *const cratonvm_types::ObjectHeader);
    if table_header.kind() != cratonvm_types::ObjectKind::Array
        || table_header.element_type() != ArrayElementType::Reference
    {
        return None;
    }
    let len = table_header.array_length() as usize;
    if len == 0 || !len.is_power_of_two() {
        return None;
    }
    let hash = tl_load_int_field(heap, raw, this, l.hash_slot)?;
    let mut i = (hash as u32 as usize) & (len - 1);
    for _ in 0..len {
        // An empty slot: the key is not in the table.
        let entry = tl_load_ref_element(heap, raw, table, i)??;
        // An element that is not an `Entry` is nothing this helper may read
        // slot 0 of as a referent (an autobox wrapper, a corrupt word): the
        // bytecode decides.
        if heap.class_id_of(entry) != l.entry {
            return None;
        }
        // `Reference.referent` is slot 0 (`REF_FIELD_REFERENT`). Compared,
        // never handed out: this is `refersTo(key)`, so no SATB keep-alive
        // is owed (`jit_getfield_body` logs the referent only because it
        // RETURNS it to compiled code).
        match tl_load_ref_field(heap, raw, entry, 0)? {
            Some(referent) => {
                if referent.as_ptr() == this.as_ptr() {
                    return Some(entry);
                }
            }
            // A cleared key: `expungeStaleEntry` is the bytecode's business.
            None => return None,
        }
        i = if i + 1 < len { i + 1 } else { 0 };
    }
    None
}

/// The raw-displacement reference-field read of the `ThreadLocal` helpers
/// (nio21 wave 17 L2, `CRATONVM_JIT_TL_RAW_FIELDS`): `Some(None)` a null
/// field, `Some(Some(r))` a reference, `None` decline.
///
/// With `raw` on and a COMPACT body, the field's displacement comes from
/// [`jit_compact_field_slot`] -- the proof cache the JIT's own `getfield`
/// helper reads, keyed by the receiver's class id, the field index and the
/// layout generation, so a redefinition or a re-registered layout is never
/// believed -- and the slot is read through
/// [`VmHeap::load_ref_slot_barriered`], the collector's load barrier (plain
/// `read_ref_slot` on Generational, G1 and an unarmed ZGC; the healing barrier
/// on an armed ZGC heap; the compressed-oop width either way). That is the
/// read `jit_getfield`'s compact-reference arm makes. What the generic
/// `get_field` additionally does for a reference slot is unbox an
/// `AUTOBOX_CLASS_ID` wrapper; this declines on one instead.
///
/// Otherwise (switch off, a legacy body) it is the generic accessor, as before.
///
/// # Safety
///
/// `obj` is a live object of this heap, and no safepoint can intervene
/// before the caller stops using what this returns (the helpers allocate
/// nothing and never reach one).
#[inline(always)]
unsafe fn tl_load_ref_field(
    heap: &VmHeap,
    raw: bool,
    obj: ObjectRef,
    slot: usize,
) -> Option<Option<ObjectRef>> {
    if raw {
        if let Some((off, storage)) = jit_compact_field_slot(obj.as_ptr() as i64, slot as i64) {
            if !storage.is_reference() {
                return None;
            }
            // SAFETY: `off` is the compact displacement of reference field
            // `slot` in `obj`'s own registered layout, so the slot lies inside
            // the live body at the current reference width.
            let word = heap.load_ref_slot_barriered(obj.as_ptr().add(off));
            return tl_checked_ref(word);
        }
        // JIT round 23 wave 1, lane L4 (w17-L2 item 2): the JIT's proof cache
        // has no entry yet (no compiled `getfield` of this field on this
        // thread); the heap's own compact reader, the same barriered load at
        // the layout's displacement, before the generic accessor.
        if let Some(word) = heap.load_ref_field_compact(obj, slot) {
            return tl_checked_ref(word);
        }
    }
    match heap.get_field(obj, slot) {
        Value::Object(v) => Some(v),
        _ => None,
    }
}

/// A loaded reference word as the `ThreadLocal` helpers may use it: null,
/// a plausible non-wrapper object, or `None` (decline).
#[inline(always)]
unsafe fn tl_checked_ref(word: u64) -> Option<Option<ObjectRef>> {
    if word == 0 {
        return Some(None);
    }
    if !cratonvm_types::plausible_heap_pointer(word) {
        return None;
    }
    // SAFETY: a plausible non-null word loaded from a live reference slot is
    // the address of a live object, whose first bytes are its header.
    if cratonvm_gc::autobox::header_is_wrapper(&*(word as *const cratonvm_types::ObjectHeader)) {
        return None;
    }
    Some(Some(ObjectRef::from_raw(word as *mut u8)))
}

/// The `int` twin of [`tl_load_ref_field`], for
/// `ThreadLocal.threadLocalHashCode`: `None` declines.
///
/// # Safety
///
/// As [`tl_load_ref_field`].
#[inline(always)]
unsafe fn tl_load_int_field(heap: &VmHeap, raw: bool, obj: ObjectRef, slot: usize) -> Option<i32> {
    if raw {
        if let Some((off, storage)) = jit_compact_field_slot(obj.as_ptr() as i64, slot as i64) {
            if !matches!(storage, cratonvm_types::FieldStorageKind::Int) {
                return None;
            }
            // SAFETY: a 4-byte `Int` field at its compact displacement inside
            // the live body; `Relaxed` through an atomic of the field's own
            // width, as `field_fast`'s `load_u32` and `read_compact_field` do.
            let p = obj.as_ptr().add(off) as *const std::sync::atomic::AtomicU32;
            return Some((*p).load(std::sync::atomic::Ordering::Relaxed) as i32);
        }
    }
    match heap.get_field(obj, slot) {
        Value::Int(h) => Some(h),
        _ => None,
    }
}

/// Element `i` of the reference array `table` (already checked to be one,
/// with `i < length`), through the collector's load barrier when `raw` is on,
/// else through the generic accessor. As [`tl_load_ref_field`] for the answer.
///
/// # Safety
///
/// `table` is a live reference array of this heap and `i` is below its length.
#[inline(always)]
unsafe fn tl_load_ref_element(
    heap: &VmHeap,
    raw: bool,
    table: ObjectRef,
    i: usize,
) -> Option<Option<ObjectRef>> {
    if raw {
        // SAFETY: the caller checked the kind, the element type and the bound;
        // the element lies at `ARRAY_DATA_OFFSET + i * ref_element_size()`,
        // the address `jit_aaload_body` reads (it spells the offset
        // `HEADER_SIZE`, which `ARRAY_DATA_OFFSET` equals).
        let slot = table
            .as_ptr()
            .add(cratonvm_types::ARRAY_DATA_OFFSET + i * ref_element_size());
        return tl_checked_ref(heap.load_ref_slot_barriered(slot));
    }
    match heap.get_array_element(table, i) {
        Ok(Value::Object(v)) => Some(v),
        _ => None,
    }
}

/// `Some(Some(value))` the value, `Some(None)` a null value, `None` declines.
unsafe fn tl_get_fast(
    vm: &SharedVm,
    mirror: ObjectRef,
    this: ObjectRef,
) -> Option<Option<ObjectRef>> {
    let l = tl_get_layout(vm)?;
    let entry = tl_find_entry(vm, &l, mirror, this)?;
    tl_load_ref_field(
        &vm.mem.heap,
        crate::runtime::env_cache::jit_tl_raw_fields(),
        entry,
        l.value_slot,
    )
}

/// `R21TlsubRows tl-set-override`: the thin `set` helper and its private-body
/// miss route ask the receiver's own `set(Object)` override before storing
/// (text pins: the helper cannot be driven without compiled code).
#[cfg(test)]
mod tl_set_override_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    #[test]
    fn the_thin_set_helper_declines_a_set_override_before_it_stores() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn jit_thread_local_set_direct_body(");
        let ask = body
            .find("tl_set_receiver_ok(")
            .expect("the override is asked");
        let find = body.find("tl_find_entry(").expect("the probe");
        let store = body.find("heap.set_field(entry").expect("the store");
        assert!(ask < find && find < store);
        let miss = body_of(&src, "\nunsafe fn tl_miss_private_ok(");
        assert!(miss.contains("if for_set && !tl_set_receiver_ok(vm, &l, cid)"));
        let ok = body_of(&src, "\nfn tl_set_receiver_ok(");
        assert!(ok.contains("!tl_class_declares_set(vm, l, cid)"));
    }
}

#[cfg(test)]
mod tl_raw_field_tests {
    /// nio21 wave 17 L2: the raw readers' word screen. Null is a null field;
    /// an unaligned or out-of-range word declines (the bytecode decides)
    /// rather than being dereferenced for the wrapper test.
    #[test]
    fn tl_checked_ref_screens_null_and_implausible_words() {
        // SAFETY: none of these words is dereferenced: 0 returns before the
        // header read and the others fail `plausible_heap_pointer` first.
        unsafe {
            assert!(matches!(super::tl_checked_ref(0), Some(None)));
            assert!(super::tl_checked_ref(0x7).is_none());
            assert!(super::tl_checked_ref(0x1001).is_none());
            assert!(super::tl_checked_ref(1u64 << 60).is_none());
        }
    }
}

/// Whether the real-field `ThreadLocal` natives are the registered ones in this
/// VM. Env-derived and process-stable (the registrar reads the same switches
/// once at registration), so one memoised bool; the policy half is per VM.
fn thread_local_real_fast_serves(vm: &SharedVm) -> bool {
    static ENV_ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    let env_on = cratonvm_jit::memo_flag::memo_bool(&ENV_ON, || {
        cratonvm_native_builtins::phases_early::thread_local_real_fast_active(true)
    });
    env_on && crate::vm::dispatch_policy(vm).is_jdk_only()
}

/// Thin direct-call target for JIT `invokevirtual ThreadLocal.set(Object)` /
/// `InheritableThreadLocal.set(Object)` sites (SBR round 2, wave 8, lane
/// `tlocal`): overwrites the value of an entry that is already in the current
/// thread's real table, exactly as `ThreadLocalMap.set`'s found-key arm does
/// (`e.value = value`), with the SATB pre-barrier on the overwritten reference
/// and the heap's own card-marking store. Everything else (no map, key absent
/// or stale, a class the helper must not answer for, tracing on, a value that
/// does not validate) declines to the generic dispatch.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_local_set_direct(
    vm_ptr: i64,
    receiver: i64,
    value: i64,
) -> i64 {
    let f = || jit_thread_local_set_direct_body(vm_ptr, receiver, value);
    contain(
        "jit_thread_local_set_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_thread_local_set_direct`], run under its panic guard.
unsafe fn jit_thread_local_set_direct_body(vm_ptr: i64, receiver: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    // `(receiver, thread mirror)` once both validated: the miss path below runs the
    // JDK's private body with them.
    let mut miss: Option<(ObjectRef, ObjectRef, Value)> = None;
    'fast: {
        if (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
            break 'fast;
        }
        if !thread_local_real_fast_serves(vm) {
            break 'fast;
        }
        let Some(this) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let new_value = if value == 0 {
            Value::Object(None)
        } else {
            if (value as u64 & 0x7) != 0 || (value as u64) >= (1u64 << 48) {
                break 'fast;
            }
            let Some(object) = vm.mem.heap.is_object_address(value as usize) else {
                break 'fast;
            };
            Value::Object(Some(object))
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let Some(mirror) = thread.java_thread_obj else {
            break 'fast;
        };
        let Some(l) = tl_get_layout(vm) else {
            break 'fast;
        };
        if l.trace {
            break 'fast;
        }
        // JIT round 21 (lane tlsub, wave 21; correctness, unconditional): a
        // receiver whose class overrides `set(Object)` takes the generic
        // (virtual) dispatch, never the store nor the private miss arm.
        if !tl_set_receiver_ok(vm, &l, vm.mem.heap.class_id_of_validated(this)) {
            break 'fast;
        }
        let Some(entry) = tl_find_entry(vm, &l, mirror, this) else {
            miss = Some((this, mirror, new_value));
            break 'fast;
        };
        let heap = &vm.mem.heap;
        // `ThreadLocalMap.set`'s found-key arm: `e.value = value`. The SATB
        // pre-barrier names the OLD reference first, as `jit_putfield_object`
        // does; `set_field` is the compact-aware store with the card barrier.
        //
        // JIT round 23 wave 1, lane L4 (w17-L2 item 3,
        // `CRATONVM_JIT_TL_SET_SINGLE_SATB`): only where the store accessor
        // does not log the old value itself -- Generational, while its queue
        // is active. G1's and ZGC's `set_field` read and log it while marking,
        // so the explicit pre-read was a second log (and a field read on every
        // call while nothing marks), as the interpreter's `aastore` already
        // skips it on G1.
        let explicit_pre = !crate::runtime::env_cache::jit_tl_set_single_satb()
            || (heap.is_generational() && heap.satb_armed());
        if explicit_pre {
            let old = heap.get_field(entry, l.value_slot);
            if let Value::Object(Some(_)) = old {
                heap.satb_barrier(old);
            }
        }
        heap.set_field(entry, l.value_slot, new_value);
        return 0;
    }
    if let Some((this, mirror, _new_value)) = miss {
        if tl_miss_private_ok(vm, this, true) {
            let args = [receiver, mirror.as_ptr() as i64, value];
            return jit_invoke_dispatch(
                vm_ptr,
                &THREAD_LOCAL_SET_PRIVATE_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                3,
            );
        }
    }
    let args = [receiver, value];
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_LOCAL_SET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Thin direct-call target for JIT `invokevirtual ThreadLocal.get()` /
/// `InheritableThreadLocal.get()` sites (SBR round 2, wave 8, lane `tlocal`).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_local_get_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_thread_local_get_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_thread_local_get_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_thread_local_get_direct`], run under its panic guard.
unsafe fn jit_thread_local_get_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // The JEP-358 action the dispatch this helper stands in for records.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let mut miss: Option<(ObjectRef, ObjectRef)> = None;
    'fast: {
        if (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
            break 'fast;
        }
        if !thread_local_real_fast_serves(vm) {
            break 'fast;
        }
        let Some(this) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let Some(mirror) = thread.java_thread_obj else {
            break 'fast;
        };
        match tl_get_fast(vm, mirror, this) {
            Some(Some(object)) => {
                // Object-return handoff root, same contract as every other thin
                // helper that hands a reference to compiled code.
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(None) => return 0,
            None => miss = Some((this, mirror)),
        }
    }
    if let Some((this, mirror)) = miss {
        if tl_miss_private_ok(vm, this, false) {
            let args = [receiver, mirror.as_ptr() as i64];
            return jit_invoke_dispatch(
                vm_ptr,
                &THREAD_LOCAL_GET_PRIVATE_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                2,
            );
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &THREAD_LOCAL_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, opt-in): the LEAF TWIN of
/// [`jit_thread_local_get_direct`]: its `'fast` arm verbatim (`tl_get_fast`
/// over the current thread's REAL table: field and array reads, the per-thread
/// layout memo, the class store under a read lock -- "allocates nothing and
/// cannot reach a safepoint"), and `-1` -- never an aligned reference -- where
/// the helper would raise the null receiver's NPE or dispatch (a miss, the
/// private-get route, a receiver class it must not answer for). The contract
/// is [`jit_preconditions_check_index_leaf`]'s.
///
/// The helper also sets the object-return handoff root
/// (`native_pending_return`) and its wrapper drops it at once
/// (`native_return_handed_to_compiled_code`); the twin does neither, which
/// leaves the same state behind: the value is reachable from the thread's own
/// `threadLocals` table, and the tail site returns it with no safepoint between.
///
/// Published through the well-known channel only where the fast arm can serve
/// at all (`thread_local_real_fast_serves`: `--jdk-only` with the real-field
/// natives), so a `--compatible` site keeps its ordinary call instead of a twin
/// that would always decline.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_thread_local_get_leaf(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || {
        if receiver == 0 || (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
            return -1;
        }
        // SAFETY: as in `jit_thread_local_get_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        if !thread_local_real_fast_serves(vm) {
            return -1;
        }
        let Some(this) = vm.mem.heap.is_object_address(receiver as usize) else {
            return -1;
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            return -1;
        };
        let Some(mirror) = thread.java_thread_obj else {
            return -1;
        };
        match tl_get_fast(vm, mirror, this) {
            Some(Some(object)) => object.as_ptr() as i64,
            Some(None) => 0,
            None => -1,
        }
    };
    contain("jit_thread_local_get_leaf", OnPanic::RecordFor { vm_ptr }, -1, f)
}

/// Synthetic call-site infos for the exact-HashMap thin direct-call helpers'
/// fallback/dispatch paths (perf/halfgap-20260717).
static HASHMAP_PUT_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/HashMap",
    method_name: "put",
    descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};
static HASHMAP_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/HashMap",
    method_name: "get",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};
static CONCURRENT_HASHMAP_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/concurrent/ConcurrentMap",
    method_name: "get",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 2,
    declaring_class_id: 0,
    owner_class_id: 0,
};

static STRING_LATIN1_LOWER_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/StringLatin1",
    method_name: "toLowerCase",
    descriptor: "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Every registry triple this file can reach through a **thin direct-call
/// helper**, i.e. every slot whose `invocations` column is a floor rather than
/// a total once the JIT is wired in this VM.
///
/// A site `jit::try_compile` binds to one of these helpers reaches the native's
/// semantics without ever redeeming a `NativeMethodId`, so
/// [`NativeMethodRegistry::record_invocation`] is never called for it. Only the
/// helpers' own `jit_invoke_dispatch` fallbacks count, and a warm site does not
/// take them. MEASURED
/// (`docs/feature-designs/jdk-only-mode.md`
/// §2): 100,000 `HashMap.put` calls report **2,000** with the JIT on against
/// **100,000** under `--nojit`, and the JIT figure does not move with the
/// workload size or with `CRATONVM_JIT_THRESHOLD` — it is frozen at the count
/// reached before the enclosing loop was compiled.
///
/// **This list is the source-verified sweep of this file, and it is longer than
/// the two helpers `G33-1` §8 N1 nominated.** `jit_integer_value_of_direct`,
/// `jit_integer_int_value_direct` and `jit_thread_current_thread_direct` return
/// from their fast arms without counting too. `G33-1` §2's remark that "the
/// `Integer` siblings in the same file route through `call_integer_native_raw`"
/// is true of the `jit_invoke_dispatch` route ONLY: the standalone direct
/// helpers are separate bodies that never enter that wrapper. The same section
/// read `StringLatin1.toLowerCase` counting exactly and concluded "the
/// direct-call family is not uniformly broken"; the body of
/// `jit_string_latin1_to_lower_direct` contains no census call at all, so that
/// probe's exact figure must have come from sites that never bound the helper,
/// not from the helper counting.
///
/// Both `ConcurrentMap.get` and `ConcurrentHashMap.get` are listed. They are
/// two registry slots holding the same `native_chm_get`;
/// `CONCURRENT_HASHMAP_GET_DIRECT_INFO` names the interface while
/// `jit_concurrent_hashmap_get_direct`'s fast arm only fires for an exact
/// `ConcurrentHashMap` receiver, so the row that loses the call is whichever of
/// the two the fallback would have resolved. A triple that is not registered in
/// this VM simply does not resolve and is not marked.
///
/// [`NativeMethodRegistry::record_invocation`]: cratonvm_native_api::NativeMethodRegistry::record_invocation
const DIRECT_CALL_HELPER_NATIVES: [(&str, &str, &str); 71] = [
    // Typed VarHandle and primitive-copy helpers bypass these erased native slots.
    (
        "java/lang/invoke/VarHandle",
        "get",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "getVolatile",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "getOpaque",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "getAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/VarHandle",
        "set",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "setVolatile",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "setRelease",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "setOpaque",
        "([Ljava/lang/Object;)V",
    ),
    (
        "java/lang/invoke/VarHandle",
        "compareAndSet",
        "([Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    ),
    // `jit_integer_value_of_direct` — TLAB-allocated wrapper, and the cold arm
    // that calls `intrinsic_integer_value_of` through `safe_native_call`
    // directly. Neither counts.
    ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;"),
    // `jit_integer_int_value_direct` — heap-validated field-0 read.
    ("java/lang/Integer", "intValue", "()I"),
    // `jit_long_value_of_direct` / `jit_long_long_value_direct` — the `Long`
    // twins of the two rows above, on the same terms.
    ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;"),
    ("java/lang/Long", "longValue", "()J"),
    // `jit_hashmap_put_direct` / `jit_hashmap_get_direct` — the overlay probe
    // and the `safe_native_call_prevalidated_objects` arm below it.
    (
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    // `jit_string_latin1_to_lower_direct` — delegates to
    // `lang_string::jit_string_to_lower_case` without touching the registry.
    (
        "java/lang/StringLatin1",
        "toLowerCase",
        "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;",
    ),
    // `jit_concurrent_hashmap_get_direct` — reaches `native_chm_get` through
    // `safe_native_call_prevalidated_objects` (H7-1), which restores the funnel
    // but still carries no `NativeMethodId`, so `record_invocation` never fires
    // and this row's count stays a floor.
    (
        "java/util/concurrent/ConcurrentMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    // `jit_thread_current_thread_direct` — returns the per-thread mirror. It
    // bumps `JIT_FUNNEL_BYPASS_HITS`, which is a different instrument: that
    // counter answers "did compiled code take this path", not "how many
    // Java-level calls did this registry slot serve".
    ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
    // SBR2 W8 `jit_thread_local_get_direct` -- the hot arm answers from the real
    // `Thread.threadLocals` table with no dispatch, so no registry call counts it.
    ("java/lang/ThreadLocal", "get", "()Ljava/lang/Object;"),
    (
        "java/lang/InheritableThreadLocal",
        "get",
        "()Ljava/lang/Object;",
    ),
    ("java/lang/ThreadLocal", "set", "(Ljava/lang/Object;)V"),
    (
        "java/lang/InheritableThreadLocal",
        "set",
        "(Ljava/lang/Object;)V",
    ),
    // ---------------------------------------------------------------
    // 2026-09-22. The nine rows above were this file's population when
    // the list was written; eight thin helpers have been added since and
    // none of them added a row, so eight families of census counts have
    // been reported as exact totals while a compiled site bypassed them.
    //
    // Swept from the `JitInvokeInfo` statics in this file, which is what
    // `every_thin_direct_call_helper_is_on_the_census_bypass_list` now
    // enumerates, so a ninth helper with a new info and no row here is a
    // red test rather than a silently exact-looking row.
    //
    // The `--jdk-only` half is new and is why this was noticed. Before
    // 2026-09-22 `jit_direct_helper_refused` declined every one of the
    // guarded helpers in strict mode, so the call took
    // `jit_invoke_dispatch` and the tail counted it -- a strict run's
    // census for these rows really was exact, which is how
    // `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
    // could read `MessageDigest.update (B)V` at one dispatch per call and
    // prove the refusal ran the native anyway. Now that the helpers serve
    // in both modes, both modes need the floor.
    // `jit_md_update_byte_direct` -- the triple that page measures.
    ("java/security/MessageDigest", "update", "(B)V"),
    // `jit_dbb_get_byte_direct` / `jit_dbb_put_byte_direct`.
    ("java/nio/ByteBuffer", "get", "(I)B"),
    ("java/nio/ByteBuffer", "put", "(IB)Ljava/nio/ByteBuffer;"),
    // `jit_buffer_session_direct`. Its FIELD-READ arm answers
    // without the registry at all, which is a bypass of exactly the
    // shape this list exists for.
    (
        "java/nio/Buffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    // `jit_preconditions_check_index_direct` -- the in-range arm
    // returns without dispatching; the throwing arm does dispatch, so
    // this row is a floor rather than a zero.
    (
        "jdk/internal/util/Preconditions",
        "checkIndex",
        "(IILjava/util/function/BiFunction;)I",
    ),
    // `jit_reachability_fence_direct` -- a `black_box` and nothing
    // else, so it never reaches the registry.
    (
        "java/lang/ref/Reference",
        "reachabilityFence",
        "(Ljava/lang/Object;)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getByteInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putByteInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V",
    ),
    // The eight `ScopedMemoryAccess.*Unaligned` helpers and their
    // eight `*Internal` twins, all served by
    // `scoped_memory_{read,write}_bytes`.
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getIntUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getShortUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getLongUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getCharUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putIntUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putShortUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putLongUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putCharUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getIntUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getShortUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getLongUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getCharUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putIntUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putShortUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putLongUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putCharUnalignedInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    ),
    // The eight `jit_unsafe_*_direct` scalar accessors. Served
    // unconditionally in both modes since 2026-09-18 -- these methods
    // have no bytecode to shadow -- and never counted.
    (
        "jdk/internal/misc/Unsafe",
        "getByte",
        "(Ljava/lang/Object;J)B",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getShort",
        "(Ljava/lang/Object;J)S",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getInt",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLong",
        "(Ljava/lang/Object;J)J",
    ),
    // SBR2 W8: the two reference-array reads.
    (
        "jdk/internal/misc/Unsafe",
        "getReference",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getReferenceVolatile",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putByte",
        "(Ljava/lang/Object;JB)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putShort",
        "(Ljava/lang/Object;JS)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putInt",
        "(Ljava/lang/Object;JI)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLong",
        "(Ljava/lang/Object;JJ)V",
    ),
    // 2026-09-24: `jit_unsafe_cas_{int,long}_direct` -- the served arm is a
    // hardware CAS with no registry call.
    (
        "jdk/internal/misc/Unsafe",
        "compareAndSetInt",
        "(Ljava/lang/Object;JII)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndSetLong",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    // JIT round 17 wave 1, lane nio: `jit_object_get_class_direct` -- the
    // served arm reads the mirror table with no registry call (opt-in bind).
    ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
    // nio21 wave 3, lane L4: the ten `HttpClient`-census rows. Their served
    // arms call the `NativeContext` accessors directly, never the registry.
    (
        "jdk/internal/misc/Unsafe",
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putReferenceVolatile",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("jdk/internal/misc/Unsafe", "fullFence", "()V"),
    (
        "java/lang/ref/Reference",
        "refersTo0",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/ref/PhantomReference",
        "refersTo0",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/Thread",
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
    ),
];

/// Declare every [`DIRECT_CALL_HELPER_NATIVES`] slot's `invocations` count
/// incomplete for `shared`, so the census labels those rows a floor instead of
/// leaving the reader to know this file exists.
///
/// # Why here, and why not a per-call counter
///
/// This runs from [`build_helpers_opt`], at the point the helper addresses are
/// published into `cratonvm_jit`'s `*_DIRECT_FN` cells — the closest thing this
/// side of the boundary has to a bind event, and the last moment before a
/// compiled site can be pointed at one of these bodies. It is cold by
/// construction: a JIT compile is orders of magnitude more expensive than the
/// forty triple hashes below, and the latch makes even those once per (VM,
/// registry generation).
///
/// The alternative — `record_invocation` inside each helper — was measured and
/// rejected. `G33-1` §5: **+9.2 ns/call** against a 1.25 ns baseline, ~6% of the
/// ~141 ns Rust native-call boundary but plausibly *the entire margin* a thin
/// direct-call helper exists to buy. Paying it here would partly undo
/// `perf/halfgap-20260717`. A bit set once and read only at report time costs
/// nothing per call and turns a number that lies into a number that says "at
/// least".
///
/// # What the bit claims
///
/// That a bypassing path is **wired** for the slot, not that a bypassing
/// dispatch has **happened**. That is the statically true statement, and it is
/// the one that survives: whether a given compile binds a given site is a fact
/// no report-time reader can recover. Under `--nojit` nothing calls
/// `build_helpers*` at all, so those runs keep an unmarked — and, for these
/// slots, exact — census.
///
/// # The latch
///
/// Keyed on `(vm_identity, registry generation)`, the same pair
/// `jit_invoke_dispatch`'s site cache uses, so a `RegisterNatives` that adds one
/// of these triples after the first compile is picked up on the next one. Races
/// between two compiling threads can only cause the marking to be *repeated*,
/// never skipped for a VM that was never marked: the VM's latch word is stored
/// after the marks, so observing it means some thread has already marked that VM.
/// `mark_invocations_incomplete` is idempotent and sticky, so a repeat is a
/// no-op.
///
/// nio21 wave 8 L2: the latch was three process statics (`MARKED_ANY`,
/// `MARKED_VM`, `MARKED_GENERATION`) keyed on the LAST VM marked, so two VMs
/// compiling in turn re-marked on every compile. It is now one word per VM,
/// `JitRealm::direct_helper_gates`' marked-generation latch.
fn mark_direct_call_helper_natives_incomplete(shared: &crate::vm::SharedVm) {
    let registry = &shared.natives.native_methods;
    let generation = registry.generation();
    let latch = &shared.jit.direct_helper_gates;
    if latch.helper_natives_marked_at(generation) {
        return;
    }
    mark_direct_call_helper_natives_incomplete_in(registry);
    latch.note_helper_natives_marked(generation);
}

/// The registry half of [`mark_direct_call_helper_natives_incomplete`], split
/// out so the marking can be driven against a registry built in a test rather
/// than only through a live `SharedVm` and a JIT compile.
///
/// Returns how many of [`DIRECT_CALL_HELPER_NATIVES`] resolved in this registry.
/// A triple that does not resolve is not an error: `register_collections_natives`
/// is a separate registrar from `register_essential_natives`, and a VM that
/// registered neither still gets a correct — empty — set of marks.
fn mark_direct_call_helper_natives_incomplete_in(
    registry: &cratonvm_native_api::NativeMethodRegistry,
) -> usize {
    let mut marked = 0usize;
    for &(class_name, method_name, descriptor) in DIRECT_CALL_HELPER_NATIVES.iter() {
        if let Some(id) = registry.resolve_id(class_name, method_name, descriptor) {
            registry.mark_invocations_incomplete(id);
            marked += 1;
        }
    }
    marked
}

thread_local! {
    static CONCURRENT_HASHMAP_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
    /// `(vm_key, class_id)` of the EXACT `java/util/HashMap` class, learned
    /// on the first direct-call fallback resolution — same pattern as
    /// `MATCHER_CLASS_CACHE`/`INTEGER_WRAPPER_CLASS_CACHE`.
    static HASHMAP_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Shared receiver/key screening for the two HashMap thin helpers. Returns
/// the validated `(thread-independent)` pieces or `None` → caller falls back
/// to the full dispatcher. The receiver must be EXACTLY `java/util/HashMap`
/// (a LinkedHashMap receiver at a HashMap-declared site takes the fallback,
/// which performs real virtual dispatch), and no class redefine may be in
/// flight (the overlay + class-id cache assume stable identity).
///
/// SAFETY: `receiver` must be non-null and 8-aligned below 2^48 (checked by
/// callers before the raw header read).
unsafe fn jit_hashmap_receiver_is_exact(vm: &SharedVm, receiver: i64) -> bool {
    let cid = std::ptr::read(receiver as usize as *const u32);
    let vm_key = vm.vm_identity;
    if HASHMAP_CLASS_CACHE.with(|c| c.get() == Some((vm_key, cid))) {
        return !class_was_redefined(vm, ClassId::new(cid));
    }
    let is_exact = vm
        .classes
        .class_manager
        .read()
        .get_class(ClassId::new(cid))
        .map(|class| class.name.as_ref() == "java/util/HashMap")
        .unwrap_or(false);
    if is_exact {
        HASHMAP_CLASS_CACHE.with(|c| c.set(Some((vm_key, cid))));
        return !class_was_redefined(vm, ClassId::new(cid));
    }
    false
}

// SAFETY: `receiver` must be a non-null pointer to a live object header, and
// `vm` must be live. The first statement reads the class id straight out of
// header+0 with `ptr::read`, so a null or stale receiver is undefined
// behaviour. Callers are JIT fast paths that have already null-checked the
// receiver at the dispatch site.
unsafe fn jit_concurrent_hashmap_receiver_is_exact(vm: &SharedVm, receiver: i64) -> bool {
    let cid = std::ptr::read(receiver as usize as *const u32);
    let vm_key = vm.vm_identity;
    if CONCURRENT_HASHMAP_CLASS_CACHE.with(|c| c.get() == Some((vm_key, cid))) {
        return !class_was_redefined(vm, ClassId::new(cid));
    }
    let exact = vm
        .classes
        .class_manager
        .read()
        .get_class(ClassId::new(cid))
        .map(|class| class.name.as_ref() == "java/util/concurrent/ConcurrentHashMap")
        .unwrap_or(false);
    if exact {
        CONCURRENT_HASHMAP_CLASS_CACHE.with(|c| c.set(Some((vm_key, cid))));
    }
    exact && !class_was_redefined(vm, ClassId::new(cid))
}

/// Guarded direct path for `ConcurrentMap.get(Object)` when the runtime
/// receiver is exactly ConcurrentHashMap. All other receivers retain the
/// canonical interface dispatcher.
///
/// # H7-1: one entry, one wrapper, one fallback
///
/// Two things in this body used to differ from its own sibling
/// [`jit_hashmap_get_direct`], for no stated reason, and both differences were
/// invisible to every arm because nothing runs a map lookup cold and warm and
/// diffs the two answers (`docs/known-issues/jdk-only/H7-1-*`):
///
/// 1. **An unrecognised key address answered `null` instead of falling back.**
///    The old `else { return 0 }` below turned "this argument is not an object
///    I can find in the heap" into the Java-visible answer `null`. Interpreted,
///    the identical call goes through the full dispatcher — so the same source
///    line could answer differently once its caller tiered up. The sibling
///    already took the other branch (`None => break 'fast`) and says why:
///    the arena-membership probe "is the only check between a stale/fabricated
///    key argument and that dereference". It is now `break 'fast` here too, so
///    the two helpers give one answer to one question. This is also the shape
///    `chm-get-misses-stored-key-in-process-RETIRED-20260804.md` records:
///    reporting ABSENT for a pair that was never compared.
/// 2. **`native_chm_get` was called with no funnel at all.** The comment that
///    stood here argued the wrapper was "second, redundant" because the native
///    pins its own receiver and key. Pinning is a *service of the funnel* —
///    `pin_native_root` writes into the ring `safe_native_call_impl` sets a
///    watermark on and truncates — and the funnel also supplies the
///    `NativeRunning` transition (which is what makes the STW census WAIT for a
///    native holding raw `ObjectRef`s in Rust locals), the argument-forwarding
///    barrier, the JNI pending-exception drain and `catch_unwind`.
///    `native_chm_get` is not a leaf by any of `set_leaf`'s four criteria: it
///    reaches `chm_key_hash`, which dispatches the key's real `hashCode()`.
///    Its sibling `native_chm_contains_key` carries a comment about exactly
///    this hazard being caught live by `RMapGcStress`. So the call now goes
///    through `safe_native_call_prevalidated_objects`, which is what the
///    sibling helper already used for its own non-leaf fallback
///    (`native_hashmap_get_exact`) three hundred lines below.
///
/// **Cost of (2), stated rather than measured** — no binary carrying this has
/// been built. It adds one native funnel entry per compiled `ConcurrentMap.get`
/// that reaches the native. The in-tree figure for the funnel-plus-by-name
/// round trip is ~400 ns/call
/// (`native-call-funnel-per-call-floor-item2-20260805.md`); this restores only
/// the funnel half, since the callback is still resolved at compile time. The
/// native it fronts is itself measured at 6920 cycles/call on the path that
/// misses its string-node cache (see `native_chm_get`'s own comment in
/// `native-collections/src/lib.rs`), so the wrapper is a small addition to a
/// large number — and the wrapper-free variant only ever appeared on the JIT
/// side, meaning every interpreted `ConcurrentHashMap.get` in this VM has
/// always paid it.
pub(in crate::jit) unsafe extern "C" fn jit_concurrent_hashmap_get_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
) -> i64 {
    let f = || jit_concurrent_hashmap_get_direct_body(vm_ptr, receiver, key);
    let bits = contain(
        "jit_concurrent_hashmap_get_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_concurrent_hashmap_get_direct`], run under its panic guard.
unsafe fn jit_concurrent_hashmap_get_direct_body(vm_ptr: i64, receiver: i64, key: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    'fast: {
        // H12-1. Both rows this ladder touches —
        // `java/util/concurrent/ConcurrentMap.get` at the call site and
        // `java/util/concurrent/ConcurrentHashMap.get`, whose `native_chm_get`
        // actually runs — are `bridge`. No compile door binds this helper
        // today (the OSR ladder does not recognise `ConcurrentMap`, and
        // `direct_native_helper_for_impl` refuses it at MethodEntry), so this
        // is the one guard here that is a backstop rather than a live fix.
        // It is written anyway, because H7-1 §4a's point is that the pair is
        // right by a coincidence of tagging that a retag is meant to change.
        //
        // After the receiver screens, for the counter reason spelled out in
        // `jit_hashmap_get_direct`.
        if receiver == 0
            || (receiver as u64 & 0x7) != 0
            || (receiver as u64) >= (1u64 << 48)
            || !jit_concurrent_hashmap_receiver_is_exact(vm, receiver)
        {
            break 'fast;
        }
        if jit_direct_helper_refused(vm, &CONCURRENT_HASHMAP_GET_GATE) {
            break 'fast;
        }
        // The `JdkConcurrentHashMap` row (interpreter round i3 wave 76, lane
        // L1; default off): the map is the JDK's, its entries are in `table`,
        // and `native_chm_get` / `jit_chm_get` read the segmented store the row
        // never builds (an empty answer). The generic dispatch below runs the
        // `ConcurrentMap.get` door, which hands the map to its own body. One
        // per-VM registry read, after the receiver screens.
        if vm
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
        {
            break 'fast;
        }
        let Some(recv) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let key_val = if key == 0 {
            // An explicit `null` key ARGUMENT, which `native_chm_get` answers
            // with `chm_reject_null_key`'s NPE. Do not confuse it with the
            // unrecognised-address case below — that one is not an argument
            // value at all, it is a failed validation.
            Value::Object(None)
        } else if (key as u64 & 0x7) != 0 || (key as u64) >= (1u64 << 48) {
            break 'fast;
        } else {
            match vm.mem.heap.is_object_address(key as usize) {
                Some(object) => Value::Object(Some(object)),
                // See item 1 in this function's doc comment. Was `return 0`.
                None => break 'fast,
            }
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        // r9w11 chm11: a primitive-wrapper or `String` key is answered by the
        // native's own allocation-free, never-blocking prefix, without the funnel.
        if let Value::Object(Some(_)) = key_val {
            let probe = {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::jit_chm_get(&mut ctx, recv, key_val)
            };
            match probe {
                Some(Value::Object(Some(object))) => {
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                Some(Value::Object(None)) => return 0,
                _ => {}
            }
        }
        let values = [Value::Object(Some(recv)), key_val];
        // Item 2 in this function's doc comment: the SAME funnel the
        // interpreter uses for the SAME callback, so the two tiers cannot
        // disagree about pinning, transitions or exception draining. Every
        // `Value::Object` in `values` was validated against this VM's heap
        // immediately above, which is exactly the precondition the
        // `_prevalidated_objects` variant is documented against.
        let result = crate::vm::safe_native_call_prevalidated_objects(
            vm,
            thread,
            cratonvm_native_collections::native_chm_get,
            &values,
        );
        match result {
            Ok(Some(Value::Object(Some(object)))) => {
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Ok(Some(Value::Object(None))) | Ok(None) => return 0,
            // A non-object `Value` out of an object-typed map is
            // out-of-contract. `get` is a read, so re-running it through the
            // dispatcher is idempotent and costs only a second lookup — the
            // reason the PUT sibling may NOT do the same thing.
            Ok(_) => break 'fast,
            Err(error) => {
                return handle_jit_dispatch_error(
                    vm,
                    thread,
                    error,
                    &CONCURRENT_HASHMAP_GET_DIRECT_INFO,
                )
            }
        }
    }
    let args = [receiver, key];
    jit_invoke_dispatch(
        vm_ptr,
        &CONCURRENT_HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Thin direct-call target for JIT `invokevirtual HashMap.get(Object)` sites
/// whose constant-pool class is exactly `java/util/HashMap` (recognition in
/// `jit::try_compile`; registered via `set_hashmap_get_direct_fn`).
///
/// Fast path: the Integer-keyed overlay probe (`jit_overlay_hashmap_get`) —
/// no Java-heap allocation, no Java dispatch, no `safe_native_call` wrapper
/// (same contract as `jit_integer_int_value_direct`). Everything else —
/// subclass receiver, non-Integer key, materialized map, redefine window —
/// falls back to the full generic dispatcher, byte-for-byte the semantics
/// the non-direct site had.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_hashmap_get_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
) -> i64 {
    let f = || jit_hashmap_get_direct_body(vm_ptr, receiver, key);
    let bits = contain(
        "jit_hashmap_get_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_hashmap_get_direct`], run under its panic guard.
unsafe fn jit_hashmap_get_direct_body(vm_ptr: i64, receiver: i64, key: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    'fast: {
        let rraw = receiver as u64;
        if (rraw & 0x7) != 0 || rraw >= (1u64 << 48) {
            break 'fast;
        }
        if !jit_hashmap_receiver_is_exact(vm, receiver) {
            break 'fast;
        }
        // H12-1. `java/util/HashMap.get` is `bridge`, so under `JdkOnly` the
        // real `java.util.HashMap` bytecode is authoritative and this helper
        // must not answer. `break 'fast` is the route every non-exact receiver
        // already takes, and `jit_invoke_dispatch` below is policy-checked.
        //
        // Placed AFTER the receiver screens on purpose, so
        // `jit_direct_helper_jdk_only_refusals` counts calls the fast path
        // would otherwise have SERVED. In front of them it would also count
        // every call that was going to `break 'fast` anyway, and a counter that
        // inflates is a counter nobody can read a ratio out of.
        if jit_direct_helper_refused(vm, &HASHMAP_GET_GATE) {
            break 'fast;
        }
        let key_val = if key == 0 {
            Value::Object(None)
        } else {
            let kraw = key as u64;
            if (kraw & 0x7) != 0 || kraw >= (1u64 << 48) {
                break 'fast;
            }
            // The key is dereferenced for the first time downstream, by
            // `unbox_wrapper`'s `class_id_of`. Keep the arena-membership probe:
            // it is the only check between a stale/fabricated key argument and
            // that dereference.
            match vm.mem.heap.is_object_address(key as usize) {
                Some(object) => Value::Object(Some(object)),
                None => break 'fast,
            }
        };
        // `jit_hashmap_receiver_is_exact` just read this live receiver's class
        // header and matched exact `HashMap`, so a second arena membership
        // probe cannot add safety on this path.
        let recv_obj = ObjectRef::from_raw(receiver as usize as *mut u8);
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let probe = {
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_overlay_hashmap_get(&ctx, recv_obj, key_val)
        };
        match probe {
            Some(Ok(Some(Value::Object(Some(object))))) => {
                // Object-return handoff root (see `jit_integer_value_of_direct`).
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(Ok(Some(Value::Object(None)))) | Some(Ok(None)) => return 0,
            // The overlay stores whatever Value was put; an object-typed map
            // returning a non-object Value is out-of-contract — take the
            // full dispatcher rather than guessing an encoding.
            Some(Ok(Some(_))) => break 'fast,
            Some(Err(error)) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_GET_DIRECT_INFO)
            }
            None => {
                // r9w11 chm11: probed AFTER the overlay, which answers only keys
                // that unbox to an `int` -- this cache holds `String` keys only, so
                // the two are disjoint and an `Integer` key no longer pays for it.
                // The native HashMap path maintains a GC-remapped, modCount-guarded
                // String-node cache. Probe it directly here so hot dynamic lowercase
                // keys avoid entering the GC-safe native wrapper at all.
                if let Value::Object(Some(key_object)) = key_val {
                    let cached = {
                        let mut ctx = crate::vm::NativeContextImpl {
                            shared: vm,
                            thread: &mut *thread,
                        };
                        ctx.hashmap_string_node_cache_get_object(recv_obj, key_object)
                    };
                    if let Some(Value::Object(Some(object))) = cached {
                        thread.native_pending_return = Some(object);
                        return object.as_ptr() as i64;
                    }
                    if let Some(Value::Object(None)) = cached {
                        return 0;
                    }
                }
                let values = [Value::Object(Some(recv_obj)), key_val];
                match crate::vm::safe_native_call_prevalidated_objects(
                    vm,
                    thread,
                    cratonvm_native_collections::native_hashmap_get_exact,
                    &values,
                ) {
                    Ok(Some(Value::Object(Some(object)))) => {
                        let result = object.as_ptr() as i64;
                        thread.native_pending_return = Some(object);
                        return result;
                    }
                    Ok(Some(Value::Object(None))) | Ok(None) => return 0,
                    Ok(_) => break 'fast,
                    Err(error) => {
                        return handle_jit_dispatch_error(
                            vm,
                            thread,
                            error,
                            &HASHMAP_GET_DIRECT_INFO,
                        )
                    }
                }
            }
        }
    }
    let args = [receiver, key];
    jit_invoke_dispatch(
        vm_ptr,
        &HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Compact-Latin1 sibling: `StringLatin1.toLowerCase(String, byte[], Locale)`,
/// so `value` is the receiver's backing array (unused — the implementation
/// reads the String) and `locale` the third argument.
///
/// Both entries delegate to `lang_string::jit_string_to_lower_case`, the same
/// implementation the interpreted native uses. They used to carry a private copy
/// of the ASCII/Unicode fold that ignored `locale` entirely, so a
/// `toLowerCase(TURKISH)` call silently changed its answer when its caller
/// tiered up — the interpreter said `tıtle`, the compiled code `title`.
///
/// That fix was one layer too shallow: the delegate called `string_case_impl`,
/// BELOW the null-`Locale` check, so `toLowerCase((Locale) null)` threw
/// interpreted and answered the default-locale string compiled. Measured on
/// OpenJDK 25.0.3+9, the oracle throws on the 200 000th warm iteration too.
/// See `docs/feature-designs/jit-ir-optimizations.md`.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_string_latin1_to_lower_direct(
    vm_ptr: i64,
    source: i64,
    _value: i64,
    locale: i64,
) -> i64 {
    let f = || jit_string_latin1_to_lower_direct_body(vm_ptr, source, _value, locale);
    let bits = contain(
        "jit_string_latin1_to_lower_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_string_latin1_to_lower_direct`], run under its panic guard.
unsafe fn jit_string_latin1_to_lower_direct_body(
    vm_ptr: i64,
    source: i64,
    _value: i64,
    locale: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    // Round 11 (lane `rt`): every shape this fast arm cannot serve goes to the
    // generic dispatcher, as `jit_hashmap_get_direct`'s does. These arms used
    // to `return 0` — a null String the Java method never produces for these
    // inputs (a null `str` with a null `Locale` must throw; an unvalidatable
    // pointer or a missing JIT thread is this helper's problem, not the
    // program's). Called only before `jit_thread_mut` is taken below, so the
    // dispatcher can take it itself.
    let dispatch = || {
        let args = [source, _value, locale];
        jit_invoke_dispatch(
            vm_ptr,
            &STRING_LATIN1_LOWER_DIRECT_INFO as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            3,
        )
    };
    if source == 0 {
        return dispatch();
    }
    let Some(source) = vm.mem.heap.is_object_address(source as usize) else {
        return dispatch();
    };
    // A `locale` of 0 is an explicit `null` Locale ARGUMENT, not "no argument":
    // the only descriptor bound to this helper,
    // `StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)`, has a
    // MANDATORY third parameter, so "absent" is not a state this site can be
    // in. `lang_string::jit_string_to_lower_case` now goes through the same
    // `string_case_native` the interpreted native does and throws for it —
    // E18-1 §1. It used to sit one layer below that check and answer the
    // default-locale string, so the same source line threw while interpreted
    // and stopped throwing once its caller tiered up.
    let locale_obj = if locale == 0 {
        None
    } else {
        // A non-zero address the heap does not recognise gets the same
        // treatment `source` gets six lines above: bail, do not guess.
        let Some(obj) = vm.mem.heap.is_object_address(locale as usize) else {
            return dispatch();
        };
        Some(obj)
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        return dispatch();
    };
    // Scoped so `thread` can be re-borrowed for the error path below, exactly
    // as `jit_hashmap_get_direct` does.
    let values = [
        Value::Object(Some(source)),
        Value::Object(None),
        Value::Object(locale_obj),
    ];
    let outcome = crate::vm::safe_native_call(
        vm,
        thread,
        cratonvm_native_builtins::lang_string::native_string_latin1_to_lower_case,
        &values,
    );
    let result = match outcome {
        Ok(Some(Value::Object(Some(object)))) => object,
        Ok(Some(Value::Object(None))) | Ok(None) => return 0,
        Ok(Some(_)) => return 0,
        // The JDK's NullPointerException for a null Locale arrives here. The
        // old code could only report it as `None`, i.e. as a null String.
        Err(error) => {
            return handle_jit_dispatch_error(vm, thread, error, &STRING_LATIN1_LOWER_DIRECT_INFO)
        }
    };
    thread.native_pending_return = Some(result);
    result.as_ptr() as i64
}

/// PUT sibling of [`jit_hashmap_get_direct`] — see its doc for the contract.
///
/// # H7-1: one implementation, and no arm that mutates and then re-dispatches
///
/// This used to call `cratonvm_native_collections::jit_overlay_hashmap_put`
/// — a `pub` re-export of `try_hm_int_fast_put` — as its own private first
/// stage, and hand every other outcome back to `jit_invoke_dispatch`. Two
/// consequences, the second of which is a wrong answer:
///
/// * It was a **second entry into the map's write path** that the registry has
///   never heard of, which is the population `H4-1` §1c and `H0-3` are about.
///   `native_hashmap_put_exact`'s FIRST statement is that same
///   `try_hm_int_fast_put`, so calling the native loses no fast path — it
///   loses the private door.
/// * `Some(Ok(Some(<non-object Value>)))` fell to `break 'fast`, i.e. to
///   `jit_invoke_dispatch`, **after the overlay insert had already happened**.
///   The generic path then re-executed the put and returned the value this
///   call had just written as if it were the previous mapping.
///   `HashMap.put`'s return value is the previous mapping, so that is a
///   silently wrong answer, and it is reachable: the overlay's stored `Value`
///   comes from whoever wrote it, and `H4-1` §1a counts 42 direct Rust
///   `native_map_put_pub` call sites that are not obliged to store an object.
///   The GET sibling may re-dispatch on this arm because a read is idempotent;
///   a write is not, and the two were written as if they were the same shape.
///
/// The exact-`java/util/HashMap` receiver guard above is what makes
/// `native_hashmap_put_exact` the right callee rather than the registered
/// `native_map_put`: for that receiver `native_map_put_evict`'s
/// `is_bare_java_lang_object` / `is_unmod_wrapper` / `ht_reject_null_*` /
/// CHM / LHM / TreeMap ladder is all no-ops, and its `CF_EXACT_HASHMAP` arm is
/// `native_hashmap_put_exact`'s body line for line. That is the same identity
/// `hashmap_native_callback` already relies on for the site-cache door, and
/// the same shape [`jit_hashmap_get_direct`]'s own fallback arm already uses.
///
/// **Cost: that paragraph's falsifier fired (r9w12).** It used to read "Cost,
/// stated rather than measured — no binary carrying this has been built. The
/// overlay-servable put now pays one `safe_native_call_prevalidated_objects`
/// funnel entry it did not pay before ... Falsified by an A/B on one binary
/// showing a map-put-dominated workload regressing beyond noise." It was
/// measured on the 2026-09-20 dev build and it does regress: `BoxProbe put`
/// (10 M `HashMap<Integer,Integer>.put`, cached boxes) put **22.67 %** of its
/// samples in `safe_native_call_impl`, more than twice the 10.57 % in
/// `try_hm_int_fast_put` — the work the funnel was wrapping.
///
/// So the overlay probe is back in front of the funnel, without the defect
/// that removed it: see the r9w12 block in the body for why the arm that
/// inserted and then re-dispatched cannot recur, and why a `None` from the
/// probe means nothing was written. The funnel still serves every non-overlay
/// outcome, which is where `map_hash_key` (arbitrary Java `hashCode()`) lives,
/// so nothing that needed the pin ring, the `NativeRunning` transition or the
/// exception drain has lost them.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_hashmap_put_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
    value: i64,
) -> i64 {
    let f = || jit_hashmap_put_direct_body(vm_ptr, receiver, key, value);
    let bits = contain(
        "jit_hashmap_put_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_hashmap_put_direct`], run under its panic guard.
unsafe fn jit_hashmap_put_direct_body(vm_ptr: i64, receiver: i64, key: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    'fast: {
        let rraw = receiver as u64;
        if (rraw & 0x7) != 0 || rraw >= (1u64 << 48) {
            break 'fast;
        }
        if !jit_hashmap_receiver_is_exact(vm, receiver) {
            break 'fast;
        }
        // H12-1. `java/util/HashMap.put` is `bridge`; see the sibling comment
        // in `jit_hashmap_get_direct`, including why this sits after the
        // receiver screens. This one matters more than the read: the OSR door
        // binds it, and a put is a MUTATION, so a strict-mode execution of this
        // body writes through CratonVM's native into a map the interpreter
        // reads with real bytecode.
        if jit_direct_helper_refused(vm, &HASHMAP_PUT_GATE) {
            break 'fast;
        }
        let mut vals = [Value::Object(None); 2];
        for (slot, raw) in [(0usize, key), (1usize, value)] {
            if raw == 0 {
                continue;
            }
            let bits = raw as u64;
            if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
                break 'fast;
            }
            // Key and value are dereferenced downstream for the first time
            // (`unbox_wrapper`'s `class_id_of` on the key, and the value once
            // it is read back out). Keep the arena-membership probe: it is the
            // only check between a stale/fabricated argument and that
            // dereference.
            match vm.mem.heap.is_object_address(raw as usize) {
                Some(object) => vals[slot] = Value::Object(Some(object)),
                None => break 'fast,
            }
        }
        // Exact-class screening above already dereferenced and validated this
        // live HashMap receiver.
        let recv_obj = ObjectRef::from_raw(receiver as usize as *mut u8);
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        // r9w12: the overlay probe, in front of the funnel and with the H7-1
        // defect kept out.
        //
        // H7-1 removed a `jit_overlay_hashmap_put` prefix from this body for
        // two reasons, and only the SECOND of them was about the prefix
        // existing: the `Some(Ok(Some(<non-object Value>)))` arm fell through
        // to `jit_invoke_dispatch` AFTER the overlay insert had already
        // happened, so the generic path re-applied the put and returned the
        // value this call had just written as the previous mapping. That is a
        // wrong answer and it is not reintroduced here: every `Some(..)` arm
        // below RETURNS, and the non-object arm answers `0` -- byte for byte
        // what the funnel arm below already answers for the same shape.
        //
        // The first reason was "a second entry into the map's write path the
        // registry has never heard of". The GET sibling has carried exactly
        // that shape since perf/halfgap-20260717 and is the precedent: the
        // probe IS `try_hm_int_fast_put`, i.e. `native_hashmap_put_exact`'s
        // own first statement, so there is one implementation of the overlay
        // insert and this is a second CALLER of it, not a second copy.
        //
        // The contract for calling it outside `safe_native_call` is
        // `jit_overlay_hashmap_put`'s own: the overlay insert touches only the
        // Rust-side shard table (whose entries the GC scans as roots via
        // `gc_overlay_roots_for_collection`), so it allocates nothing on the
        // Java heap, dispatches no Java and cannot throw. Nothing can move
        // across it, which is what lets the arguments go unpinned here -- the
        // same argument `jit_overlay_hashmap_get` makes.
        //
        // A `None` means the overlay wrote NOTHING: every `return None` in
        // `try_hm_int_fast_put` is ahead of its `entries.insert`, and the one
        // that leaves a default-constructed (empty) state behind is idempotent
        // -- the funnel's own call re-takes the same branch. So the fallback
        // below re-runs a put that has not happened yet.
        //
        // MEASURED, and this is the A/B `jit_hashmap_put_direct`'s "Cost,
        // stated rather than measured" paragraph asked for: on the 2026-09-20
        // dev build, `BoxProbe put` (10 M `HashMap<Integer,Integer>.put` of
        // cached boxes) spent **22.67 %** of its samples in
        // `safe_native_call_impl` -- the funnel entry that paragraph said an
        // overlay-servable put "now pays ... it did not pay before" -- against
        // 10.57 % in `try_hm_int_fast_put` itself. That is the regression
        // beyond noise it named as its own falsifier.
        let overlay = {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_overlay_hashmap_put(
                &mut ctx, recv_obj, vals[0], vals[1],
            )
        };
        match overlay {
            Some(Ok(Some(Value::Object(Some(object))))) => {
                // Object-return handoff root (see `jit_integer_value_of_direct`).
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(Ok(Some(Value::Object(None)))) | Some(Ok(None)) => return 0,
            // Out-of-contract, and the H7-1 §2c arm: the put has ALREADY been
            // applied, so this must NOT re-dispatch. `0` is the same reply the
            // funnel arm below gives for the same shape.
            Some(Ok(Some(_))) => {
                debug_assert!(
                    false,
                    "java/util/HashMap.put overlay returned a non-object Value; a non-Java \
                     writer stored a primitive under this key (H4-1 §1a)"
                );
                return 0;
            }
            Some(Err(error)) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_PUT_DIRECT_INFO)
            }
            // Nothing written -- the general path below is still the whole put.
            None => {}
        }
        // H7-1: the ONE implementation, through the same funnel the
        // interpreter uses for the same receiver. `native_hashmap_put_exact`
        // opens with `try_hm_int_fast_put`, so the overlay fast path is
        // unchanged; what is gone is the JIT's private entry into it and the
        // arm that inserted and then re-dispatched. Every `Value::Object` in
        // `vals` was validated against this VM's heap above, which is the
        // `_prevalidated_objects` precondition.
        let values = [Value::Object(Some(recv_obj)), vals[0], vals[1]];
        match crate::vm::safe_native_call_prevalidated_objects(
            vm,
            thread,
            cratonvm_native_collections::native_hashmap_put_exact,
            &values,
        ) {
            Ok(Some(Value::Object(Some(object)))) => {
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Ok(Some(Value::Object(None))) | Ok(None) => return 0,
            // Out-of-contract: an object-typed map answered with a non-object
            // `Value`. The put has ALREADY been applied by the call above, so
            // re-dispatching would apply it twice and return the value this
            // call wrote instead of the previous mapping — the defect this
            // rewrite exists to remove. There is no `L`-shaped encoding of a
            // primitive `Value`, so answer `null`: it is the one reply that
            // does not mutate a second time, and it is what the same match
            // already answers for `Value::Object(None)`.
            Ok(Some(_)) => {
                debug_assert!(
                    false,
                    "java/util/HashMap.put returned a non-object Value; a non-Java writer \
                     stored a primitive under this key (H4-1 §1a)"
                );
                return 0;
            }
            Err(error) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_PUT_DIRECT_INFO)
            }
        }
    }
    let args = [receiver, key, value];
    jit_invoke_dispatch(
        vm_ptr,
        &HASHMAP_PUT_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// §4 census wrapper around [`call_integer_native_raw_inner`].
///
/// `Some` is "this edge delivered the native's semantics" — which includes the
/// two arms that deliberately never touch `entry.callback` (the TLAB wrapper
/// allocation and the direct `intValue` field read). Those are open-coded
/// copies of the registered intrinsic, so from the census's point of view the
/// native was dispatched; not counting them would leave exactly the kind of
/// unverifiable path the acceptance criterion is written against. Every `None`
/// return is an argument-shape guard that runs before any of it.
#[inline]
fn call_integer_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    entry: IntegerNativeDispatchCache,
) -> Option<i64> {
    let result = call_integer_native_raw_inner(vm, thread, info, args_slice, entry);
    if result.is_some() {
        count_jit_native_dispatch(vm, entry.native_id);
    }
    result
}

#[inline]
fn call_integer_native_raw_inner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    entry: IntegerNativeDispatchCache,
) -> Option<i64> {
    if args_slice.len() != 1 {
        return None;
    }
    if matches!(entry.kind, IntegerNativeKind::ValueOf) {
        let value = args_slice[0] as i32;
        // Round 11 (lane `rt`): beyond the box cache means beyond
        // `IntegerCache.high`, not beyond 127 — the same bound, and the same
        // "unresolved means take the native" rule, as
        // `jit_integer_value_of_direct`. This twin fresh-allocated everything
        // past 127, so with `-Djava.lang.Integer.IntegerCache.high=1000`,
        // `Integer.valueOf(500) == Integer.valueOf(500)` was true interpreted
        // and false through this compiled route.
        let beyond_cache = value < -128
            || cratonvm_native_builtins::lang_math::integer_cache_high_for(vm.vm_identity)
                .is_some_and(|high| value > high);
        if beyond_cache {
            let vm_key = vm.vm_identity;
            let cached_class = INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache
                    .get()
                    .filter(|(cached_vm, _)| *cached_vm == vm_key)
                    .map(|(_, raw)| ClassId::new(raw))
            });
            if let Some(class_id) = cached_class {
                // Young-pressure relief for this cached fast path, which
                // deliberately bypasses the `safe_native_call` boundary (and
                // its young-pressure GC hook): the only argument here is a
                // primitive `i32`, so initiating the orchestrated GC is
                // exactly as safe as `jit_new_object`'s slow-path GC — no raw
                // object pointers are held across it. Without this, a
                // boxing-dominated compiled loop keeps spilling wrappers into
                // old gen until `alloc_young_initialized` hard-aborts.
                if vm.mem.heap.young_spill_pressure() {
                    // `|| old_gen_needs_gc()`: same reasoning as the
                    // `safe_native_call` hook this mirrors — the young trigger
                    // cannot see pressure that has gone into old gen, which is
                    // where every spill lands. See `vm_exec.rs`.
                    if !crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
                        && (vm.mem.heap.needs_gc_for_jit_allocation()
                            || vm.mem.heap.old_gen_needs_gc())
                    {
                        // zr z3/t2: its own site name -- a pressure verdict,
                        // not a failed allocation
                        // (`gc_and_alloc::ZGC_OCCUPANCY_FORCED_SITES`).
                        crate::runtime::interpreter::maybe_gc_forced_pub_at(
                            vm,
                            thread,
                            "jit-box-pressure",
                        );
                    }
                    vm.mem.heap.clear_young_spill_pressure();
                }
                // zr z3/lt: fallible on ZGC with a collect-and-retry; a refusal
                // falls through to the native callback below, whose funnel
                // throws the OutOfMemoryError (this arm used to abort).
                if let Some(object) = jit_box_cold_alloc(vm, &mut *thread, class_id) {
                    use cratonvm_native_api::NativeContext as _;
                    let mut ctx = crate::vm::NativeContextImpl {
                        shared: vm,
                        thread: &mut *thread,
                    };
                    ctx.set_field(object, 0, Value::Int(value));
                    // Mirror `safe_native_call`'s object-return handoff root.
                    // A peer STW cannot collect this active JIT mutator until
                    // its next safepoint, but publishing the pending value
                    // preserves the existing root contract and diagnostic
                    // visibility.
                    ctx.thread.native_pending_return = Some(object);
                    return Some(object.as_ptr() as i64);
                }
            }
        }
    }
    if matches!(entry.kind, IntegerNativeKind::IntValue) {
        let raw = args_slice[0] as u64;
        if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            return None;
        }
        let object = vm.mem.heap.is_object_address(raw as usize)?;
        // `intrinsic_integer_int_value` delegates to
        // `native_wrapper_int_value`, whose complete behavior is a read of
        // wrapper field 0 and `Int`-or-zero normalization. The receiver is
        // already heap-validated above and this operation cannot allocate or
        // safepoint, so entering `safe_native_call` adds only rooting/panic/
        // dispatch overhead on every unbox in a compiled loop.
        return Some(match vm.mem.heap.get_field(object, 0) {
            Value::Int(value) => value as i64,
            _ => 0,
        });
    }
    let arg = match entry.kind {
        IntegerNativeKind::ValueOf => Value::Int(args_slice[0] as i32),
        IntegerNativeKind::IntValue => unreachable!("handled above"),
    };
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        entry.callback,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    if matches!(entry.kind, IntegerNativeKind::ValueOf) {
        if let Some(Value::Object(Some(object))) = result {
            INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm.vm_identity,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
        }
    }
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

#[inline]
fn hashmap_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;") => Some(3),
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;") => Some(2),
        _ => None,
    }
}

/// Resolve the exact-layout `HashMap` fast-path native for this site, subject
/// to JDK-only policy. Called once per call site, at cache-fill time.
///
/// The callbacks here are **hard-coded VM-side implementations**, not registry
/// lookups — which is precisely why this needed routing: nothing about the
/// bare `match` could tell whether `java/util/HashMap.put` is registered as a
/// reviewed `Intrinsic`, an ordinary `Bridge` in front of real JDK bytecode,
/// or a `SyntheticStub` that §1.3 forbids invoking outright.
/// [`admit_jit_fast_native`] asks the registry for the kind and lets
/// `resolve_native_dispatch_wave1` answer; `Compatible` keeps the exact
/// callback this function always returned.
#[inline]
fn hashmap_native_callback(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    // JIT round 20 wave 14 (lane cjavac): under the `--compatible`
    // `jdk-hash-collections` row the map is the JDK's own (`table`), the
    // `HashMap` triples are not registered (so `admit_jit_fast_native` below
    // has no kind to ask and would admit), and these natives read stand-in
    // stores the row leaves empty -- a JDK-built map's `get` answered `null`.
    // No fast path: the generic tail runs the JDK body, as the interpreter
    // does. See `hashmap_gate_under_jdk_model`. `--jdk-only` keeps its own
    // registry-kind decision below unchanged.
    if !crate::vm::dispatch_policy(vm).is_jdk_only()
        && vm
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkHashCollections)
    {
        return None;
    }
    let callback = match (info.method_name, info.descriptor) {
        ("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;") => {
            cratonvm_native_collections::native_hashmap_put_exact
        }
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;") => {
            cratonvm_native_collections::native_hashmap_get_exact
        }
        _ => return None,
    };
    admit_jit_fast_native(
        vm,
        "java/util/HashMap",
        info.method_name,
        info.descriptor,
        callback,
    )
}

#[inline]
fn matcher_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("find", "()Z") | ("start", "()I") | ("end", "()I") | ("group", "()Ljava/lang/String;") => {
            Some(1)
        }
        ("find", "(I)Z")
        | ("start", "(I)I")
        | ("end", "(I)I")
        | ("group", "(I)Ljava/lang/String;") => Some(2),
        _ => None,
    }
}

/// The pre-existing, policy-free `Matcher` fast-path lookup: the operator's
/// `native_matcher_find()` opt-out and a two-`&'static str` match, nothing
/// else. No registry access, no lock, no hashing.
///
/// This is the callback half of the decision. It is never a dispatch verdict on
/// its own — every caller pairs it with an admission, either
/// [`matcher_native_callback`] (cache-fill sites) or
/// [`admit_matcher_leaf_native`] (the per-dispatch exact-receiver leaf).
#[inline]
fn matcher_native_callback_uncached(
    info: &JitInvokeInfo,
) -> Option<cratonvm_native_api::NativeCallback> {
    if !crate::runtime::env_cache::native_matcher_find() {
        return None;
    }
    cratonvm_native_builtins::matcher_realjdk_native_callback(info.method_name, info.descriptor)
}

/// The class the eight `Matcher` leaf triples are registered under.
const MATCHER_CLASS: &str = "java/util/regex/Matcher";

/// Dense index of the (method, descriptor) pair the exact-receiver `Matcher`
/// leaf serves, or `None` for anything else.
///
/// The eight rows are exactly the triples [`matcher_native_arg_count`] admits.
/// Keep the two in step: a pair with an index but no arg count would memoize an
/// admission that `call_matcher_native_raw` then declines, and a pair with an
/// arg count but no index would dispatch uncounted — which is the defect this
/// index exists to close.
#[inline]
fn matcher_site_index(info: &JitInvokeInfo) -> Option<usize> {
    Some(match (info.method_name, info.descriptor) {
        ("find", "()Z") => 0,
        ("start", "()I") => 1,
        ("end", "()I") => 2,
        ("group", "()Ljava/lang/String;") => 3,
        ("find", "(I)Z") => 4,
        ("start", "(I)I") => 5,
        ("end", "(I)I") => 6,
        ("group", "(I)Ljava/lang/String;") => 7,
        _ => return None,
    })
}

/// The `(method_name, descriptor)` for a [`matcher_site_index`], so the memo
/// cells can be filled without re-deriving the strings from a `JitInvokeInfo`
/// that the caller may not have.
const MATCHER_SITE_TRIPLES: [(&str, &str); 8] = [
    ("find", "()Z"),
    ("start", "()I"),
    ("end", "()I"),
    ("group", "()Ljava/lang/String;"),
    ("find", "(I)Z"),
    ("start", "(I)I"),
    ("end", "(I)I"),
    ("group", "(I)Ljava/lang/String;"),
];

/// How many memo cells the exact-receiver `Matcher` leaf uses: one per
/// [`MATCHER_SITE_TRIPLES`] row, indexed by [`matcher_site_index`] and nothing
/// else (ONE CELL, ONE TRIPLE, the invariant
/// [`cratonvm_native_api::NativeCallSite`] documents and `debug_assert!`s).
///
/// nio21 wave 8 L2: the cells were a process `static` array
/// (`MATCHER_LEAF_SITES`). Correct across VMs, because a memo carries the
/// registry generation and that is banded per registry, but per-VM state in
/// a process static, so two VMs thrashed it on every regex-hot dispatch.
/// They now live per VM, `JitRealm::direct_helper_gates.matcher_leaf_site(i)`.
/// The *policy* half of the decision was already per VM
/// (`NativeRealm::matcher_leaf_admission`, see [`admit_matcher_leaf_native`]).
pub(crate) const MATCHER_LEAF_SITE_COUNT: usize = MATCHER_SITE_TRIPLES.len();

/// `NativeRealm::matcher_leaf_admission` verdicts.
const MATCHER_ADMISSION_REFUSED: u64 = 1;
const MATCHER_ADMISSION_ADMITTED: u64 = 2;

/// Dispatches actually served by the exact-receiver `Matcher` leaf, reported by
/// `CRATONVM_INTRINSIC_STATS=1` beside the other compiled-code native counters.
///
/// It exists because the claim this leaf's memo was built to fix — "this edge
/// is uncounted for the §4 census" — is a claim about a RATE, and nothing
/// measured it. Without this number, "the leaf served the call and did not
/// count it" and "the leaf declined and the generic tail served it, counting it
/// properly" are indistinguishable from the census alone: both leave the
/// registry's total looking right on the workloads where the leaf never fires.
/// A zero here on a regex-hot run says the leaf is not the path that run takes
/// — which is a different fact from the census being complete, and only this
/// counter can tell the two apart.
///
/// Per-thread since round 11 (lane `rt`): it was a process-global `AtomicU64`
/// bumped with a `lock xadd` on every leaf-served dispatch — i.e. on every
/// compiled `Matcher` call on a regex-hot workload, from every thread. See
/// [`JitCounterBlock`].
///
/// Dispatches served by the compiled exact-receiver `Matcher` leaf this run.
pub fn matcher_leaf_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.matcher_leaf_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// OSRENTRY6-2 (JIT round 19 proposal wave 7, lane interp7): this VM's OSR
/// door direct-bind refusal census, `(reason, count)` largest first, with a
/// slow-door `None` classified by its cause instead of
/// `callee-not-yet-compiled` (`jit_bridge::osr_door_bind_refusal_reasons`).
/// For the launcher's end-of-run census.
pub fn osr_door_bind_refusal_reasons(shared: &SharedVm) -> Vec<(&'static str, u64)> {
    crate::runtime::interpreter::jit_bridge::osr_door_bind_refusal_reasons(shared)
}

/// Policy admission + census handle for the exact-receiver `Matcher` leaf,
/// memoized per triple so it can run on a per-dispatch path.
///
/// # What this replaces, and why the old shape was a gap
///
/// This leaf is the one hot by-name native fast path in this file that
/// re-decides on every dispatch: deciding used to be two `&'static str`
/// compares, so the three memoized siblings' cache-fill admission had nowhere
/// to live. Wave 1 therefore hoisted the JDK-only question to a single
/// `dispatch_policy(vm).is_jdk_only()` gate and **skipped the leaf wholesale**
/// under strict. That was safe — a disabled optimization cannot invoke a
/// `SyntheticStub` — but it left two things undone, both of which this closes:
///
/// * **`Compatible` dispatched here uncounted.** Every `find`/`start`/`end`/
///   `group` served by the leaf was missing from the §4 census, and an
///   uncounted path is an unverifiable one.
/// * **Strict lost the fast route** rather than being admitted to it. These
///   natives are registered `NativeKind::Intrinsic` (see the
///   `keep_real_matcher_find_fastpath` banner in `native-builtins/src/lib.rs`),
///   so §1.4's reviewed-intrinsic exception admits them; skipping was strictly
///   conservative, not correct-by-policy.
///
/// # The cost, which is what made the old shape necessary
///
/// Warm, this is: one dense index from two `&'static str` compares, one relaxed
/// `AtomicU64` load with a generation compare (the `NativeCallSite`), and — in
/// strict mode only — one more relaxed load with a generation compare. No lock,
/// no allocation, and above all no string hashing. The three-string registry
/// hash and, in strict mode, the class-manager read lock plus hierarchy walk
/// inside `jit_fast_native_has_bytecode` are paid once per triple per registry
/// generation.
///
/// `Compatible` does not consult the per-VM memo at all: its admission is a
/// policy field read and a `Some`, so a memo would cost more than it saves.
#[inline]
fn admit_matcher_leaf_native(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    let callback = matcher_native_callback_uncached(info)?;
    let index = matcher_site_index(info)?;
    let registry = &vm.natives.native_methods;
    let id = vm.jit.direct_helper_gates.matcher_leaf_site(index).resolve(
        registry,
        MATCHER_CLASS,
        info.method_name,
        info.descriptor,
    );
    if !crate::vm::dispatch_policy(vm).is_jdk_only() {
        return Some((callback, id));
    }
    let generation = u64::from(registry.generation());
    let cell = &vm.natives.matcher_leaf_admission[index];
    let memo = cell.load(std::sync::atomic::Ordering::Relaxed);
    if memo >> 32 == generation {
        return if memo & 0xffff_ffff == MATCHER_ADMISSION_ADMITTED {
            Some((callback, id))
        } else {
            None
        };
    }
    let admitted = jdk_only_admit_jit_fast_native(
        vm,
        MATCHER_CLASS,
        info.method_name,
        info.descriptor,
        callback,
        id,
    );
    let verdict = if admitted.is_some() {
        MATCHER_ADMISSION_ADMITTED
    } else {
        MATCHER_ADMISSION_REFUSED
    };
    cell.store(
        (generation << 32) | verdict,
        std::sync::atomic::Ordering::Relaxed,
    );
    admitted
}

/// Resolve the real-layout `Matcher` fast-path native for this site, subject
/// to JDK-only policy. Called once per call site, at cache-fill time.
///
/// The `native_matcher_find()` env gate stays the FIRST question asked: it is
/// the operator's existing opt-out and JDK-only must never make a disabled
/// fast path reappear. Policy is then layered on top of that verdict, never
/// under it.
///
/// These natives are registered `NativeKind::Intrinsic` (see the long
/// `keep_real_matcher_find_fastpath` banner in `native-builtins/src/lib.rs`),
/// so §1.4's reviewed-intrinsic exception admits them and the 19x real-JDK
/// `Matcher.find` fast path survives `--jdk-only` intact on this cached route.
/// That answer comes out of the registry, not out of an assumption here —
/// which is the point.
#[inline]
fn matcher_native_callback(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    let callback = matcher_native_callback_uncached(info)?;
    admit_jit_fast_native(
        vm,
        "java/util/regex/Matcher",
        info.method_name,
        info.descriptor,
        callback,
    )
}

#[inline]
fn stringbuilder_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("append", "(I)Ljava/lang/StringBuilder;")
        | ("append", "(C)Ljava/lang/StringBuilder;")
        | ("append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;") => Some(2),
        ("toString", "()Ljava/lang/String;") | ("length", "()I") => Some(1),
        _ => None,
    }
}

/// Resolve the registered StringBuilder native for this site ONCE (the
/// registry's 3-string hash) — cached per callsite afterwards, exactly like
/// the HashMap/Matcher kinds. Returns `None` (no caching, generic dispatch)
/// when no native is registered for the triple.
///
/// This was a bare `NativeMethodRegistry::find`, which discards the
/// `NativeKind` — so it could not tell a reviewed intrinsic from a
/// `SyntheticStub` and could not honour §1.3 or §1.4. It now resolves through
/// `resolve_id` + `callback_of`, whose semantics the registry documents as
/// *identical* to `find` (descriptor-quirk fallback included), so `Compatible`
/// mode gets the same callback from the same lookup — plus the
/// `NativeMethodId` the census needs and the kind policy needs.
#[inline]
fn stringbuilder_native_callback(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    let registry = &vm.natives.native_methods;
    let id = registry.resolve_id("java/lang/StringBuilder", info.method_name, info.descriptor)?;
    let callback = registry.callback_of(id)?;
    admit_jit_fast_native_resolved(
        vm,
        "java/lang/StringBuilder",
        info.method_name,
        info.descriptor,
        callback,
        Some(id),
    )
}

/// Invoke a cached StringBuilder native from raw JIT argument slots.
#[inline]
fn call_stringbuilder_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
) -> Option<i64> {
    let expected_len = stringbuilder_native_arg_count(info)?;
    if args_slice.len() != expected_len {
        return None;
    }
    let mut values = [Value::Object(None); 2];
    values[0] = Value::Object(Some(receiver_ref));
    if expected_len == 2 {
        match info.descriptor.as_bytes().get(1) {
            // int / char parameter — the natives take Value::Int for both.
            Some(b'I') | Some(b'C') => values[1] = Value::Int(args_slice[1] as i32),
            Some(b'L') => {
                let raw = args_slice[1];
                // `append((String) null)` must append "null" — the generic
                // path (appendNull routing) owns that; don't serve it here.
                if raw == 0 {
                    return None;
                }
                let bits = raw as u64;
                if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
                    return None;
                }
                let object = vm.mem.heap.is_object_address(bits as usize)?;
                values[1] = Value::Object(Some(object));
            }
            _ => return None,
        }
    }
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        callback,
        &values[..expected_len],
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Can a `java/util/regex/Matcher` receiver reach this call site at all?
///
/// `Matcher` is `final`, so its instances reach a virtual/interface site only
/// through `Matcher` itself or a supertype that declares the leaf triples,
/// which is `MatchResult` (`Object` declares none of them). Any other static
/// class rules the exact-receiver leaf out without looking at the receiver.
#[inline]
fn matcher_leaf_site_class(info: &JitInvokeInfo) -> bool {
    info.class_name == MATCHER_CLASS || info.class_name == "java/util/regex/MatchResult"
}

#[inline]
fn is_exact_matcher_class(vm: &SharedVm, class_id: ClassId) -> bool {
    let vm_key = vm.vm_identity;
    let raw_class_id = class_id.as_u32();
    if MATCHER_CLASS_CACHE.with(|cache| cache.get() == Some((vm_key, raw_class_id))) {
        return true;
    }

    let is_exact = vm
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|class| class.name.as_ref() == "java/util/regex/Matcher")
        .unwrap_or(false);
    if is_exact {
        MATCHER_CLASS_CACHE.with(|cache| cache.set(Some((vm_key, raw_class_id))));
    }
    is_exact
}

#[inline]
fn call_object_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    entry: NativeDispatchCache,
) -> Option<i64> {
    let result = match entry.kind {
        ObjectNativeKind::HashMap => {
            call_hashmap_native_raw(vm, thread, info, receiver_ref, args_slice, entry.callback)
        }
        ObjectNativeKind::Matcher => call_matcher_native_raw(
            vm,
            thread,
            info,
            receiver_ref,
            args_slice,
            entry.callback,
            entry.native_id,
        ),
        ObjectNativeKind::StringBuilder => call_stringbuilder_native_raw(
            vm,
            thread,
            info,
            receiver_ref,
            args_slice,
            entry.callback,
        ),
    };
    // §4 census, counted on the dispatching edge.
    //
    // `Some` is exactly "the native ran": each of the three leaves returns
    // `None` only from an argument-shape guard taken BEFORE
    // `safe_native_call_prevalidated_objects`, and returns `Some` on both the
    // `Ok` and the `Err` (native threw) arms after it. Counting here rather
    // than inside each leaf keeps the census on one line instead of threaded
    // through three signatures, and cannot count a call that did not happen.
    if result.is_some() {
        count_jit_native_dispatch(vm, entry.native_id);
    }
    result
}

/// Invoke a cached HashMap bridge directly from raw JIT argument slots.
#[inline]
fn call_hashmap_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
) -> Option<i64> {
    let expected_len = match args_slice.len() {
        len @ (2 | 3) => len,
        _ => return None,
    };

    let mut values = [Value::Object(None); 3];
    values[0] = Value::Object(Some(receiver_ref));
    for (index, &raw) in args_slice[1..].iter().enumerate() {
        if raw == 0 {
            continue;
        }
        let bits = raw as u64;
        if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
            return None;
        }
        let object = vm.mem.heap.is_object_address(bits as usize)?;
        values[index + 1] = Value::Object(Some(object));
    }

    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        callback,
        &values[..expected_len],
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Invoke a cached real-layout Matcher native from raw JIT argument slots.
#[inline]
fn call_matcher_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
    native_id: Option<cratonvm_native_api::NativeMethodId>,
) -> Option<i64> {
    let expected_len = matcher_native_arg_count(info)?;
    if args_slice.len() != expected_len {
        return None;
    }

    let mut values = [Value::Object(None); 2];
    values[0] = Value::Object(Some(receiver_ref));
    if expected_len == 2 {
        values[1] = Value::Int(args_slice[1] as i32);
    }

    let call = if native_id.is_some_and(|id| vm.natives.native_methods.is_leaf_id(id))
        && cratonvm_native_builtins::matcher_realjdk_leaf_search_cached(
            vm.vm_identity,
            receiver_ref.as_ptr() as usize,
        ) {
        crate::vm::safe_native_call_leaf_prevalidated(vm, thread, callback, &values[..expected_len])
    } else {
        crate::vm::safe_native_call_prevalidated_objects(
            vm,
            thread,
            callback,
            &values[..expected_len],
        )
    };
    let result = match call {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Specialize a lambda proxy's erased `Function.apply(Object)` call when its
/// implementation is a concrete `(int) -> double` virtual/interface target.
///
/// The generic lambda route allocates vectors, reparses descriptors, and builds
/// an interpreter frame for each application. TDigest invokes this shape in
/// its quantile/cdf numeric kernels, so dispatch directly to the already-JITed
/// receiver method and only retain the Java-mandated boxed Double result.
// SAFETY: called by the JIT's adapter fold, at a run whose four members the
// caller's constant pool proves (`lambda_int_to_double_runs_proven`).
pub unsafe extern "C" fn jit_lambda_int_to_double(vm_ptr: i64, proxy_raw: i64, index: i64) -> i64 {
    let f = || jit_lambda_int_to_double_body(vm_ptr, proxy_raw, index);
    contain(
        "jit_lambda_int_to_double",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_lambda_int_to_double`], run under its panic guard.
unsafe fn jit_lambda_int_to_double_body(vm_ptr: i64, proxy_raw: i64, index: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    // r9-vmside: every arm below that used to `return f64::NAN.to_bits()`
    // ANSWERED the Java expression `((Double) f.apply(Integer.valueOf(i)))
    // .doubleValue()` with NaN — a null `f`, a `Function` that is not the one
    // lambda shape this peephole knows, a lambda whose capture is null, a
    // `get` that threw. The peephole is chosen by the call site's constant
    // pool (`cratonvm_jit::lambda_int_to_double_runs_proven`; by method NAME,
    // `Dist.quantile*`/`Dist.cdf*`, until interpreter round i1 wave 10), not
    // by proving the receiver's type, so "not the expected shape" is an
    // ordinary input, and a NaN is a silent wrong answer in a numeric kernel. Each arm now gives the Java answer: an
    // NPE for a null receiver, and the generic `apply` for everything else.
    if proxy_raw == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // Interpreter-only mode (`jvmti_requires_interpreter`): the direct route
    // below calls a compiled `get(I)D` and skips the lambda's own frames, so
    // no MethodEntry/MethodExit or breakpoint would be delivered for either.
    // The generic `apply` goes through `jit_invoke_dispatch`, which stands down
    // the same way. Interpreter round i1 wave 8, L7.
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm) {
        return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
    }
    let Some(proxy) = vm.mem.heap.is_object_address(proxy_raw as usize) else {
        return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
    };
    // Class 0 for an array (`jit_dispatch_class`): no lambda call site.
    let proxy_class_id = jit_dispatch_class_validated(vm, proxy)
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    // Read BEFORE anything below is resolved, so a memo stamped with it saw
    // every publication and definition it counts (`LambdaGetMemo`).
    let memo_stamp = lambda_get_memo_stamp(vm);
    let memo_key = (vm.vm_identity, proxy_class_id.as_u32());
    let shape_ok = match lambda_get_memo_shape(memo_key, memo_stamp) {
        Some(shape_ok) => shape_ok,
        None => {
            let impl_class_name = lambda_int_to_double_shape(vm, proxy_class_id);
            let shape_ok = impl_class_name.is_some();
            lambda_get_memo_store(
                memo_key,
                LambdaGetMemo {
                    stamp: memo_stamp,
                    impl_class_name,
                    route: None,
                },
            );
            shape_ok
        }
    };
    if !shape_ok {
        return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
    }
    let mut receiver = match vm.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        // A null capture: the generic `apply` raises the NPE its body would.
        _ => return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index),
    };
    // Class 0 for an array (`jit_dispatch_class`), which names no class, so
    // the compiled-`get` lookup below declines rather than finding the
    // COMPONENT's `get(I)D`.
    let receiver_class_id = jit_dispatch_class(vm, receiver)
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    // JVMS §5.4.6: the direct route below compiles and calls the `get(I)D` the
    // lenient walk from the receiver's class finds. Where selection against
    // the method handle's owner picks another declaration (a private hider, or
    // a public method in another package that does not override a
    // package-private `get`), the generic `apply` runs instead. The guard is
    // dropped before either route runs (the generic one dispatches Java).
    //
    // Memoized per `(proxy class, receiver class)` with the proxy's shape
    // (`LambdaGetMemo`): this resolution, the by-name `jit_cache` probe and a
    // declined compile used to be repeated on every element of the kernel.
    let compiled = match lambda_get_memo_route(memo_key, memo_stamp, receiver_class_id) {
        Some(LambdaGetRoute::Generic) => {
            return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
        }
        Some(LambdaGetRoute::Direct(compiled)) => compiled,
        None => match resolve_lambda_get_route(
            vm,
            proxy,
            proxy_class_id,
            receiver_class_id,
            &mut receiver,
            memo_key,
            memo_stamp,
        ) {
            Ok(compiled) => compiled,
            Err(proxy_word) => {
                return lambda_int_to_double_generic(vm, vm_ptr, proxy_word, index);
            }
        },
    };
    if let Some(compiled) = compiled {
        let args = [receiver.as_ptr() as i64, index as i32 as i64];
        // The scalar helper is entered by a JIT caller that already owns the
        // active-root chain entry. Mirror the established virtual-dispatch
        // cache and re-enter this leaf directly: a second guard would mutate
        // that chain (and invalidate its scan cache) for every element.
        // Finished through `lambda_get_call_then_route`: the compiled `get`'s
        // sentinel is serviced here, never handed up raw (round 12 wave 2).
        if let Some(result) = lambda_get_call_then_route(vm, receiver_class_id, &args, || {
            try_call_compiled_entry_reentrant(
                compiled.entry_ptr() as usize,
                compiled.needs_context(),
                vm_ptr,
                &args,
            )
        }) {
            return result;
        }
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return f64::NAN.to_bits() as i64;
    };
    match crate::vm::invoke_on_class_shared(
        vm,
        thread,
        receiver_class_id,
        "get",
        "(I)D",
        &[Value::Object(Some(receiver)), Value::Int(index as i32)],
    ) {
        Ok(Some(Value::Double(value))) => value.to_bits() as i64,
        // r9-vmside: `get` threw (`IndexOutOfBoundsException` from an
        // out-of-range `list.get(i)`, say). Park it and return the sentinel
        // the compiled caller's `D`-width check tests for, exactly as the
        // dispatch helper does — it used to become NaN.
        Err(error) => {
            handle_jit_dispatch_error(vm, thread, error, &LAMBDA_INT_TO_DOUBLE_APPLY_INFO)
        }
        _ => f64::NAN.to_bits() as i64,
    }
}

/// The call site [`lambda_get_call_then_route`] finishes a compiled `get(I)D`
/// under: a receiver-dispatched `get(I)D` with no constant-pool context. Every
/// service it is handed to resolves the callee from the RECEIVER's class
/// (`invoke_kind` 0; `declaring_class_id` 0, so `jit_site_owner` asks no
/// selection question: the direct route already asked it,
/// `resolve_lambda_get_route`), and its static address keeps the per-site
/// memos those services key on (`callee_handler_template_key`, the
/// virtual-target memo) on one stable identity. `class_name` only names the
/// site in diagnostics: with a receiver present nothing resolves by it.
static LAMBDA_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "get",
    descriptor: "(I)D",
    num_jit_args: 2,
    return_type: b'D',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// [`LAMBDA_GET_DIRECT_INFO`] with the receiver's class as its loader context,
/// for [`try_resume_trapped_callee`] only. That function resolves the stash's
/// declaring class through `declaring_class_id`'s loader and, at `0`, through
/// the bootstrap loader alone, which refuses every application `get` (the
/// frame is then re-run from entry instead of resumed). The receiver's class
/// sees the declaring class: it is that class or one of its supertypes. Built
/// per call, so it must never key a memo; the resume keys none.
fn lambda_get_resume_site(receiver_class_id: ClassId) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: LAMBDA_GET_DIRECT_INFO.class_name,
        method_name: LAMBDA_GET_DIRECT_INFO.method_name,
        descriptor: LAMBDA_GET_DIRECT_INFO.descriptor,
        num_jit_args: LAMBDA_GET_DIRECT_INFO.num_jit_args,
        return_type: LAMBDA_GET_DIRECT_INFO.return_type,
        invoke_kind: LAMBDA_GET_DIRECT_INFO.invoke_kind,
        declaring_class_id: receiver_class_id.as_u32(),
        owner_class_id: 0,
    }
}

/// Precise resume of a PURE deopt that a directly called compiled `get(I)D`
/// left stashed: rebuild its frame at the trap and run it to completion, as
/// the dispatch helper's compiled arms do. `None`, stash untouched, when there
/// is no stash, when a throwable is pending beside it (the general service in
/// [`lambda_get_call_then_route`] owns that shape), or when the resume refuses.
///
/// # Safety
/// As [`try_resume_trapped_callee`]: `thread` is this thread's exclusive borrow.
unsafe fn resume_trapped_lambda_get(
    vm: &SharedVm,
    thread: &mut JvmThread,
    receiver_class_id: ClassId,
) -> Option<i64> {
    if receiver_class_id.as_u32() == 0
        || !cratonvm_jit::deopt::has_last_deopt()
        || jit_pending_exception_is_set()
    {
        return None;
    }
    let site = lambda_get_resume_site(receiver_class_id);
    try_resume_trapped_callee(vm, thread, &site, receiver_class_id)
}

/// Call the compiled `get(I)D` the direct route chose through `call`, and
/// finish the call the way the dispatch helper finishes its own compiled arms
/// (`call_compiled_then_route`). `None` when `call` made no call.
///
/// Round 12 wave 2 (lane osrfix): this route used to hand the callee's raw
/// result back. A compiled `get` that TRAPPED (an uncommon trap on the branch
/// it had never taken, typically the `throw` arm of its own bounds check)
/// answers the `i64::MIN` sentinel with its reconstructed frame stashed, and
/// only the helper between the two frames can resume that frame. Handed up
/// raw, the stash reached the compiled caller's sink as a FOREIGN frame: an
/// OSR'd caller raised `InternalError` ("a callee's stashed frame reached the
/// OSR sink",
/// r12w1-orch-osr-catch-around-lambda-peephole-throws-internalerror), a
/// method-entry caller re-ran from bci 0. Now a pure deopt is resumed at its
/// trap ([`resume_trapped_lambda_get`]), and everything else -- a throw routed
/// through `get`'s own handler, a frameless trap, a declined stash re-run from
/// entry -- takes [`route_implicit_exc_through_callee`], as for any
/// receiver-dispatched compiled callee. A throw that escapes `get` stays
/// parked beside the sentinel for the caller's `D`-width check.
///
/// The receiver is pinned across the call and the routing, which can re-run
/// `get` with it after a collection moved it
/// (r11w4-rt-callee-rerun-reads-pre-call-argument-words).
///
/// # Safety
/// Called from [`jit_lambda_int_to_double_body`] with no `jit_thread_mut`
/// borrow live; `call` enters `receiver_class_id`'s live, unwrapped `get(I)D`
/// body with `args`, which is `[receiver, index]`.
unsafe fn lambda_get_call_then_route(
    vm: &SharedVm,
    receiver_class_id: ClassId,
    args: &[i64; 2],
    call: impl FnOnce() -> Option<i64>,
) -> Option<i64> {
    if !lambda_get_service_enabled() {
        return call();
    }
    let info = &LAMBDA_GET_DIRECT_INFO;
    let pins =
        jit_thread_mut().map(|(thread, _guard)| CompileArgPins::pin_raw(vm, thread, info, args));
    let routed = call().map(|rc| {
        if rc != i64::MIN {
            return rc;
        }
        if let Some(v) = jit_thread_mut()
            .and_then(|(thread, _guard)| resume_trapped_lambda_get(vm, thread, receiver_class_id))
        {
            return v;
        }
        route_implicit_exc_through_callee(vm, info, args, receiver_class_id, rc, pins.as_ref())
    });
    if let Some(pins) = pins {
        // As `call_compiled_then_route`: every nested entry restores the
        // thread pointer, so this finds it.
        if let Some((thread, _guard)) = jit_thread_mut() {
            pins.release(thread);
        }
    }
    routed
}

/// `CRATONVM_JIT_LAMBDA_GET_SERVICE` (default on; `0` hands the compiled
/// `get`'s raw result back as before round 12 wave 2, sentinel and stash
/// included). Cached once per process.
fn lambda_get_service_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LAMBDA_GET_SERVICE")
    })
}

/// The unmemoized half of [`jit_lambda_int_to_double_body`]'s route choice for
/// `receiver` (a `receiver_class_id` instance captured by `proxy`): the
/// JVMS §5.4.6 selection test, the compiled `get(I)D` probe, and the one
/// compile the first call asks for. `Ok(body)` is the direct route (`None`:
/// the interpreted `get`); `Err(proxy word)` is the generic `apply`, on the
/// proxy's current address. Records its answer in `LAMBDA_GET_MEMO` except
/// when the capture read back null after the compile. `receiver` is re-read
/// when the compile may have moved it.
// SAFETY: called only from `jit_lambda_int_to_double_body`, on this thread's
// live JIT frame (`jit_thread_mut`); `proxy` was validated by the caller.
unsafe fn resolve_lambda_get_route(
    vm: &SharedVm,
    proxy: ObjectRef,
    proxy_class_id: ClassId,
    receiver_class_id: ClassId,
    receiver: &mut ObjectRef,
    memo_key: (usize, u32),
    memo_stamp: LambdaGetStamp,
) -> Result<Option<std::sync::Arc<cratonvm_jit::CompiledMethod>>, i64> {
    let Some(impl_class_name) = lambda_get_memo_impl_class(memo_key, memo_stamp)
        .or_else(|| lambda_int_to_double_shape(vm, proxy_class_id))
    else {
        return Err(proxy.as_ptr() as i64);
    };
    let class_name = {
        let cm = vm.classes.class_manager.read();
        let name = cm.get_class(receiver_class_id).and_then(|class| {
            let selection_moves = cm
                .find_class_by_name_for_class(&impl_class_name, receiver_class_id)
                .and_then(|owner| {
                    crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                        cm.class_store(),
                        receiver_class_id,
                        owner,
                        "get",
                        "(I)D",
                    )
                })
                .is_some();
            (!selection_moves).then(|| class.name.clone())
        });
        name
    };
    let Some(class_name) = class_name else {
        lambda_get_memo_note_route(
            memo_key,
            memo_stamp,
            receiver_class_id,
            LambdaGetRoute::Generic,
        );
        return Err(proxy.as_ptr() as i64);
    };
    let mut compiled = {
        let cache = vm.jit.jit_cache.read();
        cache
            .get(&class_name, "get", "(I)D", receiver_class_id)
            // `java.util.Vector.get(int)` is `synchronized`, so this route is
            // one `Vector` receiver away from calling a monitor-bearing body
            // with no monitor. See `CompiledMethod::requires_wrapped_entry`.
            .filter(|compiled| !compiled.requires_wrapped_entry)
    };
    if compiled.is_none() {
        // This direct scalar route bypasses the normal bytecode invocation
        // counter, so publish the receiver-specialized getter once here.
        //
        // The compile may load classes, i.e. run Java and collect. `proxy` is
        // this helper's own copy, so it is pinned across the compile and
        // re-read (see [`CompileArgPins`]); `load_and_forward` was the repair
        // here, and it needs a forwarding word at the old address that ZGC's
        // slide does not leave.
        let mut pinned_proxy = [Value::Object(Some(proxy))];
        let pins = jit_thread_mut()
            .map(|(thread, _guard)| CompileArgPins::pin_values(vm, thread, &pinned_proxy));
        let _ = crate::runtime::interpreter::try_jit_compile_callee_for_class(
            vm,
            receiver_class_id,
            &class_name,
            "get",
            "(I)D",
            true,
        );
        if let Some(pins) = pins {
            if let Some((thread, _guard)) = jit_thread_mut() {
                pins.release_values(thread, &mut pinned_proxy);
            }
        }
        compiled = vm
            .jit
            .jit_cache
            .read()
            .get(&class_name, "get", "(I)D", receiver_class_id)
            // r9-vmside: the same filter as the first probe. This re-probe had
            // lost it, so the FIRST call on a cold `Vector` receiver — the one
            // that compiles `get` — entered the `synchronized` body with no
            // monitor, exactly what the comment on the first probe forbids.
            .filter(|compiled| !compiled.requires_wrapped_entry);
        // `receiver` is this helper's own copy of the proxy's field: re-read it
        // through the re-read proxy rather than call `get` on a pre-move
        // address.
        let proxy = match pinned_proxy[0] {
            Value::Object(Some(proxy)) => proxy,
            _ => proxy,
        };
        *receiver = match vm.mem.heap.get_field(proxy, 0) {
            Value::Object(Some(captured)) => captured,
            _ => return Err(proxy.as_ptr() as i64),
        };
    }
    // A declined compile is remembered too: until the stamp moves (any
    // publication, definition or redefinition), this pair takes the
    // interpreted `get` without asking the compiler again on every element.
    lambda_get_memo_note_route(
        memo_key,
        memo_stamp,
        receiver_class_id,
        LambdaGetRoute::Direct(compiled.clone()),
    );
    Ok(compiled)
}

/// The impl class of `proxy_class_id`'s lambda call site when it is the one
/// shape [`jit_lambda_int_to_double`] routes directly: a one-capture
/// `Function.apply(Object)` whose implementation is a virtual or interface
/// `get(I)D`. `None` for every other proxy.
fn lambda_int_to_double_shape(
    vm: &SharedVm,
    proxy_class_id: ClassId,
) -> Option<std::sync::Arc<str>> {
    let call_site = vm.classes.lambda_call_site_for(proxy_class_id)?;
    let shape = call_site.functional_interface.as_ref() == "java/util/function/Function"
        && call_site.sam_method_name.as_ref() == "apply"
        && call_site.sam_descriptor.as_ref() == "(Ljava/lang/Object;)Ljava/lang/Object;"
        && call_site.capture_types.len() == 1
        && call_site.impl_handle.member_name.as_ref() == "get"
        && call_site.impl_handle.descriptor.as_ref() == "(I)D"
        && matches!(
            call_site.impl_handle.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        );
    shape.then(|| call_site.impl_handle.class_name.clone())
}

/// What [`jit_lambda_int_to_double`] chose for one receiver class of one
/// lambda proxy class.
#[derive(Clone)]
enum LambdaGetRoute {
    /// The generic `apply`: selection picks another `get(I)D` than the
    /// receiver's lenient walk (JVMS §5.4.6), or the receiver's class is
    /// unknown.
    Generic,
    /// The direct route: the compiled, unwrapped `get(I)D`, or `None` (no body
    /// was published and the compile declined): the interpreted `get`.
    Direct(Option<std::sync::Arc<cratonvm_jit::CompiledMethod>>),
}

/// `(process JIT cache generation, class-definition epoch of the VM's class
/// store, the VM's redefinition epoch)`. Every answer a [`LambdaGetMemo`]
/// holds is a function of these: a compiled body is found by exact key
/// (a publication, replacement or withdrawal moves the generation), the
/// shape and the selection test by class ids and names (a definition or an
/// unload moves the epoch), and a redefinition moves the last.
type LambdaGetStamp = (u64, u64, u32);

/// One lambda proxy class's resolution in [`jit_lambda_int_to_double`], valid
/// while the stamp holds (round 12 wave 1, lane rt; item 4's first bullet of
/// `r11-rt-helper-hot-paths-repeat-work-per-call`). Before it, every element
/// of a `Dist.quantile`/`cdf` kernel re-did the call-site lookup (a read
/// lock), the selection test (the class-manager read lock and an `Arc<str>`
/// clone), a by-name `jit_cache` probe, and, when `get` could not compile, the
/// compile request itself.
struct LambdaGetMemo {
    stamp: LambdaGetStamp,
    /// `Some(impl class)` when the proxy's call site is the one shape.
    impl_class_name: Option<std::sync::Arc<str>>,
    /// The route for the last receiver class seen: these kernels are
    /// monomorphic, so one slot per proxy class is enough.
    route: Option<(u32, LambdaGetRoute)>,
}

/// Entries kept per thread before the memo is emptied (it is keyed on class
/// ids, so a program with many lambda classes must not grow it without bound).
const LAMBDA_GET_MEMO_CAP: usize = 64;

thread_local! {
    /// `(vm_identity, proxy ClassId) -> LambdaGetMemo`. Not a
    /// `site_keyed_memos!` member: no `JitInvokeInfo` address is in the key,
    /// and a class id is covered by the stamp's class-definition epoch.
    // addr-keyed: not an address -- VM identity + class id.
    static LAMBDA_GET_MEMO: std::cell::RefCell<rustc_hash::FxHashMap<(usize, u32), LambdaGetMemo>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `CRATONVM_JIT_LAMBDA_GET_MEMO`: default ON; `0` / `false` / `off`
/// resolves on every call, as before round 12.
fn lambda_get_memo_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_LAMBDA_GET_MEMO").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// The stamp, read BEFORE anything it validates is resolved.
fn lambda_get_memo_stamp(vm: &SharedVm) -> LambdaGetStamp {
    (
        cratonvm_jit::jit_cache_generation(),
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
        vm.jit.jit_cache.redefine_epoch(),
    )
}

/// The memoized shape verdict for `key`, if a memo valid at `stamp` holds one.
fn lambda_get_memo_shape(key: (usize, u32), stamp: LambdaGetStamp) -> Option<bool> {
    if !lambda_get_memo_enabled() {
        return None;
    }
    LAMBDA_GET_MEMO.with(|m| {
        m.borrow()
            .get(&key)
            .filter(|memo| memo.stamp == stamp)
            .map(|memo| memo.impl_class_name.is_some())
    })
}

/// The memoized impl class name for `key` at `stamp`.
fn lambda_get_memo_impl_class(
    key: (usize, u32),
    stamp: LambdaGetStamp,
) -> Option<std::sync::Arc<str>> {
    if !lambda_get_memo_enabled() {
        return None;
    }
    LAMBDA_GET_MEMO.with(|m| {
        m.borrow()
            .get(&key)
            .filter(|memo| memo.stamp == stamp)
            .and_then(|memo| memo.impl_class_name.clone())
    })
}

/// The memoized route for `receiver_class_id` under `key` at `stamp`.
fn lambda_get_memo_route(
    key: (usize, u32),
    stamp: LambdaGetStamp,
    receiver_class_id: ClassId,
) -> Option<LambdaGetRoute> {
    if !lambda_get_memo_enabled() {
        return None;
    }
    LAMBDA_GET_MEMO.with(|m| {
        m.borrow()
            .get(&key)
            .filter(|memo| memo.stamp == stamp)
            .and_then(|memo| memo.route.as_ref())
            .filter(|(cid, _)| *cid == receiver_class_id.as_u32())
            .map(|(_, route)| route.clone())
    })
}

/// Replace `key`'s memo.
fn lambda_get_memo_store(key: (usize, u32), memo: LambdaGetMemo) {
    if !lambda_get_memo_enabled() {
        return;
    }
    LAMBDA_GET_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= LAMBDA_GET_MEMO_CAP && !m.contains_key(&key) {
            m.clear();
        }
        m.insert(key, memo);
    });
}

/// Record `route` for `receiver_class_id` in `key`'s memo, if that memo is
/// still the one resolved at `stamp` (a nested call on this thread may have
/// replaced it with a newer one while the compile ran Java).
fn lambda_get_memo_note_route(
    key: (usize, u32),
    stamp: LambdaGetStamp,
    receiver_class_id: ClassId,
    route: LambdaGetRoute,
) {
    if !lambda_get_memo_enabled() {
        return;
    }
    LAMBDA_GET_MEMO.with(|m| {
        if let Some(memo) = m.borrow_mut().get_mut(&key) {
            if memo.stamp == stamp {
                memo.route = Some((receiver_class_id.as_u32(), route));
            }
        }
    });
}

/// Synthetic call site for [`lambda_int_to_double_generic`]: the erased
/// `Function.apply(Object)` the peephole replaced, dispatched on its receiver.
static LAMBDA_INT_TO_DOUBLE_APPLY_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/function/Function",
    method_name: "apply",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 2,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// HotSpot's JEP 358 message for `((Double) f.apply(i)).doubleValue()` when
/// `apply` returned null (`tools/probes/interp/L7/L7LambdaIntToDoubleFold.java`).
const LAMBDA_INT_TO_DOUBLE_NULL_RESULT_NPE: &str =
    "Cannot invoke \"java.lang.Double.doubleValue()\" \
     because the return value of \"java.util.function.Function.apply(Object)\" is null";

/// The Java semantics [`jit_lambda_int_to_double`] stands in for, for every
/// receiver its fast shape does not cover:
/// `((Double) f.apply(Integer.valueOf(i))).doubleValue()`.
///
/// Four steps, each with the exception the bytecode would raise: box the
/// index (`Integer.valueOf`, through its own direct helper), dispatch `apply`
/// on the receiver through the generic dispatcher (an exception it throws is
/// parked and answered with the sentinel), `checkcast Double` (a
/// `ClassCastException`), `doubleValue()` on null (an NPE). Every failure
/// returns `i64::MIN` with the signal parked, which the compiled caller's
/// `D`-width post-invoke check disambiguates through `dispatch_threw`.
///
/// # Safety
/// Called only from [`jit_lambda_int_to_double_body`], i.e. from compiled code
/// with a live `vm_ptr`. `proxy_raw` may be stale after a collection;
/// `jit_invoke_dispatch` forwards its reference arguments.
#[cold]
#[inline(never)]
unsafe fn lambda_int_to_double_generic(
    vm: &SharedVm,
    vm_ptr: i64,
    proxy_raw: i64,
    index: i64,
) -> i64 {
    // `Integer.valueOf(i)`. Its direct helper returns `i64::MIN` with the
    // exception parked when boxing fails.
    //
    // Boxing an index outside the `Integer` cache ALLOCATES, and an allocation
    // can collect and move the proxy. `proxy_raw` is this helper's own copy —
    // the compiled caller's frame slot is fixed up, this local is not — and
    // `jit_invoke_dispatch`'s argument forwarding only recovers an object
    // while its old address still holds a forwarding pointer, which fresh
    // allocation into the reclaimed space can overwrite. Pin a validated proxy
    // in the thread's `native_pin_roots` (scanned and remapped) across the box
    // and re-read it (r11-rt-lambda-proxy-raw-pointer-held-across-boxing).
    // The thread borrow is released before boxing, which takes its own.
    let pin_idx = vm
        .mem
        .heap
        .is_object_address(proxy_raw as usize)
        .and_then(|proxy| {
            jit_thread_mut().map(|(thread, _guard)| {
                let idx = thread.native_pin_roots.len();
                thread.native_pin_roots.push(proxy);
                idx
            })
        });
    let boxed = jit_integer_value_of_direct(vm_ptr, index as i32 as i64);
    let proxy_raw = match pin_idx {
        Some(idx) => match jit_thread_mut() {
            Some((thread, _guard)) => {
                let current = thread
                    .native_pin_roots
                    .get(idx)
                    .map_or(proxy_raw, |proxy| proxy.as_ptr() as usize as i64);
                thread.native_pin_roots.truncate(idx);
                current
            }
            None => proxy_raw,
        },
        None => proxy_raw,
    };
    if boxed == i64::MIN {
        return i64::MIN;
    }
    // The box is this helper's own raw local: `jit_integer_value_of_direct`
    // no longer leaves it rooted in `native_pending_return` once it returns
    // (handoff-w36b), and nothing else names it until the dispatch below has
    // forwarded its arguments into the callee's frame. Pin it across that
    // entry (its SATB flush can meet a collection), exactly as the proxy was
    // pinned across the box.
    let boxed_pin = vm.mem.heap.is_object_address(boxed as usize).and_then(|b| {
        jit_thread_mut().map(|(thread, _guard)| {
            let idx = thread.native_pin_roots.len();
            thread.native_pin_roots.push(b);
            idx
        })
    });
    let args = [proxy_raw, boxed];
    let result = jit_invoke_dispatch(
        vm_ptr,
        &LAMBDA_INT_TO_DOUBLE_APPLY_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    );
    if let Some(idx) = boxed_pin {
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.native_pin_roots.truncate(idx);
        }
    }
    if result == i64::MIN {
        // `apply` threw, or the dispatcher refused: its signal is parked.
        return i64::MIN;
    }
    if result == 0 {
        // `doubleValue()` on the null `apply` returned. The compiled frame
        // reports the bci of the fold's first instruction (the `apply`), where
        // an action-only JEP 358 message cannot be derived, so the message is
        // spelled here: HotSpot's, computed at the `invokevirtual doubleValue`.
        let Some((thread, _guard)) = jit_thread_mut() else {
            set_jit_pending_npe_action(
                crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
            );
            return i64::MIN;
        };
        if let crate::error::MethodCallFailed::ExceptionThrown(exc) =
            crate::runtime::exceptions::throw_runtime_error(
                vm,
                thread,
                crate::error::RuntimeError::NullPointerException {
                    message: Some(LAMBDA_INT_TO_DOUBLE_NULL_RESULT_NPE.to_string()),
                },
            )
        {
            set_jit_pending_exception(thread, exc);
        }
        return i64::MIN;
    }
    let Some(obj) = vm.mem.heap.is_object_address(result as usize) else {
        // Not an object the heap recognises — nothing to cast or unbox.
        return f64::NAN.to_bits() as i64;
    };
    // An array is never a `Double`, though a `Double[]`'s header names
    // `Double` (`jit_dispatch_class`); it takes the refusal below, which used
    // to read "field 0" out of the array instead.
    let obj_class_name = match jit_dispatch_class_validated(vm, obj) {
        JitDispatchClass::Class(obj_class_id) => vm
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .map(|c| c.name.to_string()),
        JitDispatchClass::JavaLangObject => Some(
            crate::runtime::interpreter::cce_display_class_name(vm, obj, "java/lang/Object"),
        ),
    };
    if obj_class_name.as_deref() == Some("java/lang/Double") {
        // `java.lang.Double` is final, so the name is the whole `checkcast`.
        if let Value::Double(d) = vm.mem.heap.get_field(obj, 0) {
            return d.to_bits() as i64;
        }
        return f64::NAN.to_bits() as i64;
    }
    // `checkcast java/lang/Double` refused.
    let Some((thread, _guard)) = jit_thread_mut() else {
        return f64::NAN.to_bits() as i64;
    };
    // JIT round 21 wave 25, lane hexname (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`):
    // the receiver by its BOTTOM class's hidden flag (a hidden class keeps its
    // `/0x<hex>` tail, an ordinary `p/0x2A` is dotted); the bare `replace`
    // stays the fallback and the `=0` text.
    let shown = obj_class_name.as_deref().unwrap_or("<unknown>");
    let message = format!(
        "class {} cannot be cast to class java.lang.Double",
        crate::runtime::exceptions::external_name_by_bottom_class_or(
            shown,
            obj_class_name.as_deref().and_then(|name| {
                crate::runtime::exceptions::bottom_class_hidden(vm, vm.mem.heap.class_id_of(obj), name)
            }),
            |name| name.replace('/', "."),
        )
    );
    if let crate::error::MethodCallFailed::ExceptionThrown(exc) =
        crate::runtime::exceptions::throw_runtime_error(
            vm,
            thread,
            crate::error::RuntimeError::ClassCastException { message },
        )
    {
        set_jit_pending_exception(thread, exc);
    }
    i64::MIN
}

/// t1 wave 4 (lane Y2): while an inline-cache publication runs under
/// `JdkOnly`, route the JIT's refusal recorder to THIS VM's
/// `--jdk-only-report` sinks.
///
/// The publication APIs (`JitMICSlot::update`, `JitPICSlot::install`) carry a
/// `jdk_only` bit and no VM, and their refusal
/// (`record_jdk_only_ic_native_refusal` in `jit/src/lib.rs`) writes to the
/// innermost `JdkOnlySinkScope` on the thread, falling back to the process
/// sinks. A mutator-thread IC miss is outside any compile scope, so before
/// this every such refusal fed one process counter that every VM's report
/// read (AGENTS.md: no process globals for compatibility state). Opening the
/// scope at each VM publication site keeps the API unchanged.
///
/// `None` under `Compatible`: the refusal branch is unreachable there (it is
/// gated on the same bit), so the thread-local write is skipped entirely. Bind
/// the result to a named `_scope` so it lives across the publication calls.
#[inline]
fn jdk_only_ic_publication_scope(
    vm: &SharedVm,
    jdk_only: bool,
) -> Option<cratonvm_jit::jdk_only_sink::JdkOnlySinkScope<'_>> {
    jdk_only.then(|| {
        cratonvm_jit::jdk_only_sink::JdkOnlySinkScope::enter(vm.jit.jit_cache.jdk_only_sinks())
    })
}

/// Give this SAM call site an inline-cache entry of its own.
///
/// The Rust arm below answers a lambda dispatch in ~190 ns; a named class's
/// call site answers one in ~12, because after its first miss the inline cache
/// holds the callee and the emitted cascade never re-enters Rust
/// (`mic_calls=1` across 2 200 000 dispatches, `CRATONVM_DBG=mic-prof`). The
/// only thing keeping a lambda out of that slot was an argument shuffle — the
/// call site has `(proxy, samArgs…)` and the impl wants `(captures…, samArgs…)`
/// — so `lambda_adapter` emits a thunk that performs the shuffle, reads any
/// captures out of the proxy, and tail-jumps; the slot holds THAT.
///
/// Two questions are asked here, and the rest belong to the emitter:
///
/// * **no `checkcast`** — a generic call site's cast is a class-hierarchy
///   question, not a register move.
/// * **a static impl**, asserted rather than assumed because the thunk drops the
///   receiver outright and nothing downstream would notice if it mattered.
///
/// Whether the captures' types, the proxy's layout and the arity permit an
/// emission is `lambda_adapter_entry`'s to decide — it is what would have to
/// emit them. A `None` from it leaves this site on the Rust arm for good,
/// because the install is claimed before the attempt rather than after it; and
/// every refusal it can make is a property of the SITE rather than of the
/// moment, so nothing is given up by asking once.
///
/// That last sentence was NOT true while a reference capture could also be
/// refused for the collector's read-barrier state, which is a property of the
/// instant: a site that happened to ask during an armed cycle lost its thunk
/// permanently. That refusal is gone — it was both inert by default and
/// unnecessary, see `lambda_adapter_entry` — so the "ask once" shape is now
/// unqualified. Claiming after the attempt instead would trade it for
/// re-asking (a lock and a layout lookup) on every dispatch a refused site ever
/// serves, which is the shape of the 202 000 re-installs
/// `LambdaJitSite::adapter_slots` exists to prevent.
///
/// Both slots are written, because the emitted cascade prefers the PIC when the
/// codegen allocated one and never consults the MIC in that case.
///
/// Everything after installation is the ordinary cached-call path: the
/// cascade's own null and `ObjectKind::Object` guards run before the call, its
/// `i64::MIN` callee-deopt check after it, and an escaping exception is left in
/// `jit_pending_exception` for the caller's post-invoke check. This arm
/// participates in none of it any more, which is the point.
#[allow(clippy::too_many_arguments)]
unsafe fn install_lambda_inline_cache(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    receiver_class_id: ClassId,
    code: &cratonvm_jit::RetainedCode,
    vm_ptr: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) {
    if !crate::runtime::env_cache::jit_lambda_adapter() {
        return;
    }
    // JIT round 20 wave 12 (lane qLam): a site with a Rust-side adapter (box /
    // unbox, native target) has no machine thunk; the direct arm serves it.
    if site.rust_only() {
        return;
    }
    // Read before the thunk is resolved, for the shared-table publication
    // below (`install_mega_dispatch_way`'s redefinition rule).
    let epoch_seen = vm.jit.jit_cache.redefine_epoch();
    if site.num_captures() != 0 && !crate::runtime::env_cache::jit_lambda_capture_adapter() {
        return;
    }
    // JIT round 20 wave 12 (lane qLam, DISP20-3): an INSTANCE-method impl
    // site carries the receiver guard its thunk bakes (the site exists only
    // under `CRATONVM_JIT_LAMBDA_INSTANCE_THUNK`); every other non-static
    // impl is refused as before.
    let receiver_guard = site.receiver_thunk_guard();
    if !site.is_static_impl() && receiver_guard.is_none() {
        return;
    }
    // nio21 w18 L2: a site that replays bridge `checkcast`s gets a CAST thunk
    // (`cratonvm_jit::lambda_adapter::lambda_cast_adapter_entry`) once every
    // cast has a class this site PROVED passes; until then nothing is claimed,
    // so a later call (whose arguments prove one) installs it. The thunk's
    // miss edge is THIS site's resolving helper call, so it is installed into
    // this site's MIC/PIC only. `CRATONVM_JIT_LAMBDA_CAST_THUNK=0` restores
    // the refusal (read here, once per claim, never per call).
    let cast_guards = if site.has_checkcasts() {
        let Some(guards) = site.cast_thunk_guards() else {
            return;
        };
        // The switch is read AFTER the per-site claim below, never here: a
        // site whose install is refused for good (an over-budget arity) comes
        // back on every call, and w18 read the flag 2.4 M times on `hdr`.
        if mic_ptr == 0 {
            return;
        }
        Some(
            guards
                .into_iter()
                .map(
                    |(sam_index, class_id)| cratonvm_jit::lambda_adapter::CastGuard {
                        sam_index,
                        class_id,
                    },
                )
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    // JIT round 20 wave 12 (lane qLam, DISP20-3): a receiver thunk always has
    // a miss edge (its receiver guard), so it needs the MIC even with no cast,
    // and is therefore always built as a guarded thunk (`cast_guards` is
    // `Some`, possibly empty -- which also keeps it out of the shared
    // megamorphic table below). An unbound receiver's own bridge cast is
    // implied by the exact-class guard and dropped; a proved class other than
    // the receiver's could never pass both, so that site gets no thunk.
    let cast_guards = match receiver_guard {
        None => cast_guards,
        Some(receiver) => {
            if mic_ptr == 0 {
                return;
            }
            let mut guards = cast_guards.unwrap_or_default();
            if receiver.source == cratonvm_jit::lambda_adapter::ReceiverSource::SamArg0 {
                if guards
                    .iter()
                    .any(|g| g.sam_index == 0 && g.class_id != receiver.class_id)
                {
                    return;
                }
                guards.retain(|g| g.sam_index != 0);
            }
            Some(guards)
        }
    };
    // A thunk tail-jumps to the impl and returns straight to its compiled
    // caller, so no Rust runs on that path and the constant-return probe
    // CANNOT see those calls. Say so -- in strict mode by refusing the thunk,
    // otherwise by counting what is going unseen, so a clean
    // `site_const_mismatch` is not mistaken for a clean bill of health.
    if site.const_return().is_some() && crate::runtime::env_cache::jit_lambda_const_probe() {
        if crate::runtime::env_cache::jit_lambda_const_probe_strict() {
            return;
        }
        crate::runtime::interpreter::const_probe_note_opaque();
    }
    if !site.claim_adapter_install(mic_ptr, pic_ptr) {
        return;
    }
    // nio21 w19 L2: the cast thunk's switch, once per claim (see above).
    // (A receiver thunk is under its own switch, read when its site was built.)
    if cast_guards.is_some()
        && receiver_guard.is_none()
        && !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LAMBDA_CAST_THUNK")
    {
        return;
    }
    // `total_args` is captures plus SAM arguments; the emitter wants them apart,
    // because only the SAM arguments arrive in registers.
    let sam_args = site.total_args() - site.num_captures();
    if let Some(receiver) = receiver_guard {
        // JIT round 20 wave 12 (lane qLam): the receiver thunk.
        let guards = cast_guards.as_deref().unwrap_or(&[]);
        let Some(entry) = cratonvm_jit::lambda_adapter::lambda_receiver_adapter_entry(
            vm.jit.jit_cache.lambda_adapters(),
            receiver_class_id.as_u32(),
            code.arc(),
            site.capture_descs(),
            sam_args,
            receiver,
            guards,
            cratonvm_jit::lambda_adapter::CastMissSite {
                helper: jit_invoke_virtual_mic as *const () as usize,
                vm_ptr: vm_ptr as usize,
                info_ptr: info as *const JitInvokeInfo as usize,
                mic_ptr: mic_ptr as usize,
                pic_ptr: pic_ptr as usize,
            },
        ) else {
            return;
        };
        install_lambda_receiver_thunk(vm, site, receiver_class_id, code, mic_ptr, pic_ptr, entry);
        return;
    }
    let entry = match &cast_guards {
        None => cratonvm_jit::lambda_adapter::lambda_adapter_entry(
            vm.jit.jit_cache.lambda_adapters(),
            receiver_class_id.as_u32(),
            code.arc(),
            site.capture_descs(),
            sam_args,
        ),
        // Cast: addresses carried as the helpers' `i64` words.
        Some(guards) => cratonvm_jit::lambda_adapter::lambda_cast_adapter_entry(
            vm.jit.jit_cache.lambda_adapters(),
            receiver_class_id.as_u32(),
            code.arc(),
            site.capture_descs(),
            sam_args,
            guards,
            cratonvm_jit::lambda_adapter::CastMissSite {
                helper: jit_invoke_virtual_mic as *const () as usize,
                vm_ptr: vm_ptr as usize,
                info_ptr: info as *const JitInvokeInfo as usize,
                mic_ptr: mic_ptr as usize,
                pic_ptr: pic_ptr as usize,
            },
        ),
    };
    let Some(entry) = entry else {
        return;
    };
    let class_id = receiver_class_id.as_u32();
    // nio21 w18 L2: a cast thunk's site comes back here on every cast MISS
    // (an argument class other than the proved one). When two such sites of
    // one proxy alternate, the per-site latch above re-claims on each miss;
    // a site whose MIC already holds exactly this thunk is left untouched
    // rather than re-stored (the MIC's first line is the one the cascade
    // loads on every call).
    if cast_guards.is_some() && mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        if mic
            .cached_class_id
            .load(std::sync::atomic::Ordering::Acquire)
            == class_id
            && mic.cached_entry().0 == entry as u64
        {
            return;
        }
    }
    let class_name = site.impl_class_name();
    let needs_ctx = code.needs_context();
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    // Run the thunk once before anything dispatches through it. See
    // `const_thunk_self_test`: for a constant-returning impl the answer is
    // known from the bytecode, so a wrong thunk is caught deterministically
    // here rather than statistically later.
    if crate::runtime::env_cache::jit_lambda_const_probe()
        && !const_thunk_self_test(site, entry, needs_ctx, vm_ptr, receiver_ref)
    {
        return;
    }
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    let mut installed = false;
    if mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        // MEGA5-18-4: a gate-first megamorphic site never reads its MIC.
        if !mic_publication_skipped(pic_ptr) {
            mic.update(class_id, class_name, entry as u64, needs_ctx, jdk_only);
        }
        installed = true;
    }
    if pic_ptr != 0 {
        let pic = &*(pic_ptr as *const JitPICSlot);
        // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`).
        let mic_serves = mic_ptr != 0
            && pic_way_redundant_with_mic(&*(mic_ptr as *const JitMICSlot), pic, class_id);
        if !mic_serves && pic.install(class_id, class_name, entry as u64, needs_ctx, jdk_only) {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(
                vm,
                crate::threading::thread_state::current_thread_id().map(crate::ThreadId),
            );
        }
        installed = true;
        // Round 12 wave 3 (lane mega2, proposal W2-1 step 3): and the VM's
        // shared megamorphic table, under this SAM site's selector, so every
        // other site of the same SAM method whose own ways overflow reaches
        // the thunk from its hashed stub instead of from this helper. The
        // thunk is a function of (proxy class, SAM name and descriptor) alone
        // (`lambda_jit_site`), both of which the key carries, and it is
        // exactly what this site's PIC just published. Only an impl without
        // an exception table: the table's Rust reader services a sentinel
        // through `handle_rust_armed_callee_sentinel`, which this arm
        // deliberately does not use for a lambda (see its sentinel note).
        // nio21 w18 L2: never a cast thunk -- its miss edge names THIS site.
        if mega_table_publish_lambda_enabled()
            && cast_guards.is_none()
            && site.cached_impl().exception_table.is_empty()
        {
            install_mega_dispatch_way(
                vm,
                info,
                Some(pic),
                class_id,
                entry as u64,
                needs_ctx,
                epoch_seen,
            );
        }
    }
    if installed {
        crate::runtime::interpreter::lambda_site_bump_adapter(site.num_captures());
        if mic_prof::enabled() {
            // The call counts AT THE MOMENT OF INSTALL. `site_adapters`
            // alone cannot tell "installed early and served" from
            // "installed after the workload was over", which is exactly
            // what an intermittent `site_direct` failure asks.
            let (fast_returns, site_direct, _) =
                crate::runtime::interpreter::lambda_jit_engagement();
            eprintln!(
                "[cratonvm-jitc] lambda-adapter installed class_id={class_id} \
                 captures={} sam_args={sam_args} entry={entry:#x} impl={} \
                 at site_direct={site_direct} fast_returns={fast_returns}",
                site.num_captures(),
                class_name,
            );
        }
    }
}

/// JIT round 20 wave 12, lane qLam (DISP20-3): publish an instance-method
/// impl site's RECEIVER thunk (`lambda_receiver_adapter_entry`) into THIS call
/// site's MIC / PIC under the proxy's class id -- the tail of
/// [`install_lambda_inline_cache`] for a cast thunk, which it is a kind of: its
/// miss edge names this site, so it is never published to the shared
/// megamorphic table, and a slot that already holds exactly this thunk is not
/// re-stored. The constant-return self-test does not apply (an instance site
/// has no constant return; see `build_lambda_instance_jit_site`).
///
/// A site whose receivers alternate between two classes holds one of their
/// thunks at a time; the other class misses into the Rust direct arm, which
/// serves it (its own site has already spent its install claim).
#[allow(clippy::too_many_arguments)]
unsafe fn install_lambda_receiver_thunk(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    proxy_class_id: ClassId,
    code: &cratonvm_jit::RetainedCode,
    mic_ptr: i64,
    pic_ptr: i64,
    entry: usize,
) {
    let class_id = proxy_class_id.as_u32();
    if mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        if mic
            .cached_class_id
            .load(std::sync::atomic::Ordering::Acquire)
            == class_id
            && mic.cached_entry().0 == entry as u64
        {
            return;
        }
    }
    let class_name = site.impl_class_name();
    let needs_ctx = code.needs_context();
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    let mut installed = false;
    if mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        if !mic_publication_skipped(pic_ptr) {
            mic.update(class_id, class_name, entry as u64, needs_ctx, jdk_only);
        }
        installed = true;
    }
    if pic_ptr != 0 {
        let pic = &*(pic_ptr as *const JitPICSlot);
        // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`).
        let mic_serves = mic_ptr != 0
            && pic_way_redundant_with_mic(&*(mic_ptr as *const JitMICSlot), pic, class_id);
        if !mic_serves && pic.install(class_id, class_name, entry as u64, needs_ctx, jdk_only) {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(
                vm,
                crate::threading::thread_state::current_thread_id().map(crate::ThreadId),
            );
        }
        installed = true;
    }
    if installed {
        crate::runtime::interpreter::lambda_site_bump_adapter(site.num_captures());
        // The positive control (`CRATONVM_DBG_MIC_PROF=1`): one line per install.
        if mic_prof::enabled() {
            let (fast_returns, site_direct, _) =
                crate::runtime::interpreter::lambda_jit_engagement();
            eprintln!(
                "[cratonvm-jitc] lambda-receiver-thunk installed class_id={class_id} \
                 receiver={:?} captures={} entry={entry:#x} impl={}.{} \
                 at site_direct={site_direct} fast_returns={fast_returns}",
                site.receiver_thunk_guard(),
                site.num_captures(),
                class_name,
                site.impl_method_name(),
            );
        }
    }
}

/// Withdraw what a dispatch just published into its inline caches if a class
/// redefinition landed while it was resolving the target.
///
/// `jit_invoke_virtual_mic` stamps the MIC with the redefinition epoch BEFORE
/// it resolves and publishes. A redefinition that lands between the stamp and
/// the publication is invisible to that stamp, and the published entry is then
/// served by the inline machine-code cascade, which never re-enters the helper
/// to notice. Re-reading the epoch after publishing closes the window: the
/// entry is withdrawn, and the next miss finds the stamp stale, flushes and
/// re-resolves against the redefined class.
fn withdraw_ic_publication_if_redefined(
    vm: &SharedVm,
    epoch_at_resolution: u32,
    mic: &JitMICSlot,
    pic: Option<&JitPICSlot>,
) {
    if vm.jit.jit_cache.redefine_epoch() == epoch_at_resolution {
        return;
    }
    mic.clear_compiled_entry();
    if let Some(pic) = pic {
        pic.clear_entries();
    }
}

/// A compiled caller's SAM call, served straight from the lambda call site's
/// own cached target.
///
/// This is the arm that gives a lambda call site what every named-class call
/// site already had. `jit_invoke_virtual_mic` is entered ONCE per named-class
/// site — after that the inline cascade in `jit/src/x64.rs` calls the callee
/// from machine code and never returns to Rust (`mic_calls=1` over 2 200 000
/// dispatches on `probes/SamHotLoopProbe.java`). A lambda receiver is caught by
/// the arm above this one, which returns before the inline cache is ever
/// reached, so its site can never be cached: `mic_calls=2197000`,
/// `hit_entry=0`, ~819 cycles a call.
///
/// The inline cache cannot hold a lambda — its cascade passes the caller's own
/// argument registers straight through, and a SAM call's registers are not the
/// impl's (the proxy receiver has to go, the captures have to arrive). So the
/// site is cached one level out, in Rust, and this arm spends it: read the
/// captures out of the proxy's fields, put the SAM's own already-decoded raw
/// arguments after them, and call the compiled impl through the same
/// reentrant primitive the monomorphic hit path uses.
///
/// Deliberately identical to the MIC hit path in everything that is not the
/// argument shuffle — same `try_call_compiled_entry_reentrant_owned`, same
/// `i64::MIN` deopt-sentinel handling, same "an escaping exception is left in
/// `jit_pending_exception` for the compiled caller's own post-invoke check".
/// A `None` from it means the callee's arity exceeds the register tables, and
/// the caller falls through to the generic path with nothing done.
///
/// See known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md.
unsafe fn try_lambda_site_direct_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    receiver_ref: ObjectRef,
    receiver_class_id: ClassId,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    vm_ptr: i64,
    ic_slots: Option<(i64, i64)>,
) -> Option<i64> {
    const MAX_DIRECT_ARGS: usize = crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS;
    // Interpreter-only mode (`jvmti_requires_interpreter`): this arm enters a
    // compiled impl, which posts no MethodEntry/MethodExit and stops at no
    // breakpoint. Declining leaves the call to the generic lambda route, whose
    // one-shot door stands down the same way. Checked before the inline-cache
    // install below, so nothing is published for the machine-code cascade
    // either. Interpreter round i1 wave 6, L7. The i54-L1 retirement: also
    // while a C JVMTI env lists lambda proxy frames, which only the generic
    // route records (`interpreter::lambda_proxy_frames_armed`).
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm)
        || crate::runtime::interpreter::lambda_proxy_frames_armed(vm)
    {
        return None;
    }
    let site = match crate::runtime::interpreter::lambda_jit_site(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
    ) {
        Some(site) => site,
        // JIT round 20 wave 12 (lane qLam, DISP20-3): an INSTANCE-method impl,
        // keyed by the call's own receiver class (`CRATONVM_JIT_LAMBDA_INSTANCE_THUNK`).
        None => crate::runtime::interpreter::lambda_instance_jit_site(
            vm,
            receiver_class_id,
            receiver_ref,
            args_slice,
            info.method_name,
            info.descriptor,
        )?,
    };
    crate::runtime::interpreter::lambda_site_bump_calls();
    // `args_slice[0]` is the proxy receiver; the SAM's own arguments follow it.
    if !site.direct_enabled() {
        // This site's body deoptimized once already; the interpreter's one-shot
        // path owns it from here (see `LambdaJitSite::direct_disabled`).
        crate::runtime::interpreter::lambda_site_bump_deopted();
        return None;
    }
    if args_slice.is_empty() || site.total_args() > MAX_DIRECT_ARGS {
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    }
    // JIT round 20 wave 12 (lane qLam; `CRATONVM_JIT_LAMBDA_SITE_BOXING`,
    // `CRATONVM_JIT_LAMBDA_SITE_NATIVE`): a site with a Rust-side adapter.
    // Its unboxed SAM arguments replace the caller's words for everything
    // below (the impl, and the handler / implicit-route rebuilds, which decode
    // by the impl's own descriptor), it never installs a machine thunk, and a
    // native target is called here and now. The caller's own `args_slice` is
    // untouched, so a decline hands the general path the boxed originals.
    let mut adapted_words = [0i64; MAX_DIRECT_ARGS + 1];
    // JIT r20 wave 13 (lane huntGC2): the caller's own words, which the
    // collector keeps current. `adapted_words` is a Rust local no collector
    // sees, so after the impl ran (a GC point) the routes below re-derive
    // their words from these (`lambda_site_args_after_call`).
    let caller_args = args_slice;
    let (args_slice, ic_slots) = match site.adapt() {
        None => (args_slice, ic_slots),
        Some(adapt) => {
            let words = adapted_words.get_mut(..args_slice.len())?;
            words.copy_from_slice(args_slice);
            for &(k, prim, wrapper) in adapt.unbox_list() {
                let raw = *args_slice.get(1 + k)?;
                if raw == 0 {
                    // LambdaMetafactory's class raises the NPE; the general
                    // path does too.
                    return None;
                }
                // SAFETY: a non-zero reference word of the compiled caller is
                // a live object, as everywhere else on this arm.
                let obj = ObjectRef::from_raw(raw as *mut u8);
                if vm.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array
                    || vm.mem.heap.class_id_of(obj) != wrapper
                {
                    // Not exactly the wrapper: the general path's cast decides.
                    crate::runtime::interpreter::lambda_site_bump_refused();
                    return None;
                }
                words[1 + k] = crate::runtime::interpreter::lambda_unbox_bits(vm, prim, obj)?;
            }
            (&adapted_words[..args_slice.len()], None)
        }
    };
    if let Some(native_info) = site.adapt().and_then(|adapt| adapt.native_info()) {
        return lambda_site_native_call(
            vm,
            thread,
            &site,
            receiver_ref,
            info,
            args_slice,
            caller_args,
            native_info,
        );
    }
    let sam_args = &args_slice[1..];
    if site.num_captures() + sam_args.len() != site.total_args() {
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    }
    let Some(code) = crate::runtime::interpreter::lambda_jit_site_code(vm, &site) else {
        // Still interpreted. The warmup counter on the generic path is what
        // gets it compiled; until then this arm has nothing to call.
        crate::runtime::interpreter::lambda_site_bump_no_code();
        return None;
    };

    if !crate::runtime::interpreter::lambda_jit_site_checkcasts_pass(vm, &site, sam_args) {
        // A cast that would throw. Decline, and let the generic path raise the
        // `ClassCastException` it has always raised, message and all.
        crate::runtime::interpreter::lambda_site_bump_refused();
        return None;
    }
    if let Some((mic_ptr, pic_ptr)) = ic_slots {
        install_lambda_inline_cache(
            vm,
            &site,
            info,
            receiver_ref,
            receiver_class_id,
            &code,
            vm_ptr,
            mic_ptr,
            pic_ptr,
        );
    }
    let mut jit_args = [0i64; MAX_DIRECT_ARGS];
    crate::runtime::interpreter::lambda_jit_site_capture_args(
        vm,
        &site,
        receiver_ref,
        &mut jit_args,
    );
    jit_args[site.num_captures()..site.total_args()].copy_from_slice(sam_args);
    let entry = code.entry_ptr() as usize;
    let needs_ctx = code.needs_context();
    let Some(rc) = try_call_compiled_entry_reentrant_owned(
        &code,
        entry,
        needs_ctx,
        vm_ptr,
        &jit_args[..site.total_args()],
    ) else {
        // The callee's arity is past what the register tables cover. Nothing
        // ran; the generic path takes it.
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    };
    if rc == i64::MIN {
        // `i64::MIN` is either a deopt sentinel or a `long` that really is
        // `Long.MIN_VALUE`, and only the out-of-band signals can tell them
        // apart. Draining them is safe here BECAUSE this arm called exactly one
        // method: whatever they say happened, happened inside this site's impl.
        //
        // The shared sentinel handler is not used for this, and the reason is
        // no longer that it CANNOT be: `try_resume_trapped_callee` used to
        // identify the trapped callee by the CALL SITE's name alone, which for
        // a SAM call is `apply` and never the `lambda$...` body that actually
        // trapped, so it refused the resume and minted an orphan
        // (`lambda-callee-deopt-is-orphaned-by-the-sam-name-check-20260908`).
        // It now asks the metafactory's `impl_handle` as a second identity and
        // would answer correctly. This arm still owns the case, because it owns
        // something that helper does not: `site.disable_direct()`, the latch
        // that keeps every LATER call to this site off the direct arm. Draining
        // the signals here is what makes that decision possible.
        let sig = take_all_jit_signals(thread);
        let stashed = cratonvm_jit::deopt::take_last_deopt_with_point();
        // Interpreter round i5 wave 103, lane L2: the impl's body was retired
        // after this site picked it, and its not-entrant stub answered the
        // call through the VM, where the throwable already left the impl
        // (`DrainedJitSignals::escaped_a_redispatch_of`). Its table must not be
        // searched again (no pc: a typed row would match by class alone).
        if stashed.is_none() && !site.cached_impl().exception_table.is_empty() && {
            let imp = site.cached_impl();
            sig.escaped_a_redispatch_of(&imp.method_name, &imp.method_descriptor)
        } {
            restash_jit_signals(thread, sig);
            crate::runtime::interpreter::lambda_site_bump_direct();
            return Some(rc);
        }
        // nio21 wave 21 L1: an EXPLICIT throwable leaving an impl with its own
        // table (admitted only under `CRATONVM_JIT_LAMBDA_HANDLER_IMPL`) is
        // offered to that table first; restashed below, it would leave the
        // impl past its `catch` / `finally`.
        if let (None, Some(exc)) = (stashed.as_ref(), sig.exception) {
            if !site.cached_impl().exception_table.is_empty() {
                // Interpreter round i5 wave 102, lane L2: the impl's own
                // stamp only (`DrainedJitSignals::athrow_bci_owned_by`).
                // (Fields, not the method: `sig.exception` is moved out.)
                let athrow_bci = {
                    let imp = site.cached_impl();
                    throw_stamp_owned_by(
                        sig.athrow_bci,
                        sig.athrow_owner,
                        method_identity_key(
                            &imp.class_name,
                            &imp.method_name,
                            &imp.method_descriptor,
                        ),
                    )
                };
                // Interpreter round i5 wave 105, lane L2: NO stamp at all
                // (not a refused foreign one) means the VM re-posted the
                // throwable after the impl's bytecode ran
                // (`JIT_THROW_PC_UNSTAMPED_EXPLICIT`).
                let throw_pc = if athrow_bci >= 0 {
                    athrow_bci as usize
                } else if sig.athrow_bci == -1 {
                    crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT
                } else {
                    usize::MAX
                };
                // JIT r20 wave 13 (lane huntGC2): the words as they are NOW.
                let mut fresh = [0i64; MAX_DIRECT_ARGS + 1];
                let routed = match lambda_site_args_after_call(vm, &site, caller_args, &mut fresh) {
                    Some(now) => lambda_route_thrown_through_impl(
                        vm, thread, &site, info, now, exc, throw_pc,
                    ),
                    None => {
                        clear_lambda_impl_exceptional_frame(vm, thread, site.cached_impl());
                        Err(exc)
                    }
                };
                let bits = match routed {
                    Ok(bits) => bits,
                    Err(exc) => {
                        set_jit_pending_exception(thread, exc);
                        i64::MIN
                    }
                };
                crate::runtime::interpreter::lambda_site_bump_direct();
                // qLam: an adapted site boxes the handler's value.
                return Some(lambda_site_adapt_return(vm, thread, &site, info, bits));
            }
        }
        if lambda_direct_sentinel_is_not_a_deopt(
            &sig,
            stashed.is_some(),
            !site.cached_impl().exception_table.is_empty(),
        ) {
            // Not a deopt: an escaping exception (which propagates through
            // `jit_pending_exception`, exactly as it does out of the MIC hit
            // path) or a genuine `Long.MIN_VALUE`. Put every signal back the
            // way it was found and hand the value on.
            restash_jit_signals(thread, sig);
            crate::runtime::interpreter::lambda_site_bump_direct();
            screen_const_return(&site, rc, "jit-direct-minsentinel");
            return Some(rc);
        }
        // An implicit exception in an impl WITH its own exception table
        // (interpreter round i1 wave 4, L7). Routed through that table at the
        // throw site, exactly as `route_implicit_exc_through_callee` does for a
        // named callee, instead of being read as a deopt: the decline below
        // hands the call to the generic path, which re-runs the impl FROM
        // ENTRY and commits everything the compiled body did before the trap a
        // second time. Neither outcome is a deopt, so the site keeps its
        // direct arm. Only a routing this function cannot decide falls through.
        //
        // Round 13 wave 8 (lane replay4): such a fall-through is not a bare
        // trap but an implicit exception whose handler could not be resumed;
        // its re-run is judged at the throw bci (`-1`: unknown) below.
        let implicit_throw_bci =
            (sig.npe || sig.aioobe.is_some() || sig.arithmetic).then_some(sig.athrow_bci);
        // JIT r20 wave 13 (lane huntGC2): an adapted site's words re-derived
        // from the caller's CURRENT words (the impl ran: a GC point); `None`
        // (cannot happen for words that adapted before the call) keeps the
        // signals for the deopt path below, as an undecided route does.
        let mut fresh = [0i64; MAX_DIRECT_ARGS + 1];
        let route_args = lambda_site_args_after_call(vm, &site, caller_args, &mut fresh);
        if let (None, Some(now)) = (stashed.as_ref(), route_args) {
            match lambda_direct_route_implicit(vm, thread, &site, info, now, sig) {
                // No constant-return screen: a body with a handler is never a
                // bare constant (`LambdaJitSite::const_return`).
                LambdaImplicitRoute::Completed(bits) => {
                    crate::runtime::interpreter::lambda_site_bump_direct();
                    // qLam: an adapted site boxes the handler's value.
                    return Some(lambda_site_adapt_return(vm, thread, &site, info, bits));
                }
                LambdaImplicitRoute::Declined => {}
            }
        }
        // A deopt. Its signals describe an attempt that is being abandoned and
        // are dropped with it, and this site leaves the direct arm for good so
        // every later call goes through the interpreter's one-shot.
        //
        // But "the body did not complete" is NOT "the body did nothing". A
        // deopt sentinel means the compiled body ran up to `rframe.bci` and
        // stopped there; returning `None` here hands the call to the generic
        // path below, which re-enters the impl FROM ENTRY and therefore
        // re-executes every side effect already committed before the trap. That
        // is the hibernate-reactive `reactiveRemove`-fires-twice defect: one
        // `ArrayLoop.next()` dispatch, two deletes, `--nojit` clean,
        // `CRATONVM_JIT_LAMBDA_SITE=0` clean. See
        // `docs/known-issues/hibernate/hib-reactive-3gc-run-regressions-20260820.md`
        // section 8.
        //
        // The reconstructed frame is what makes resuming from the trap point
        // possible, so spend it instead of dropping it — through the same
        // resume sink the interpreter's one-shot door uses, keyed on this
        // site's OWN impl identity (`site.cached_impl()`), which is exactly the
        // identity `try_resume_trapped_callee` could not supply. The stashing
        // point's address goes with it (round 11 wave 10), so a foreign frame
        // from a superseded body is not charged to the fresh one.
        //
        // Interpreter round i1 wave 15, lane L3: an impl that left at a
        // back-edge poll because the interpreter-only mode is on keeps the
        // direct arm. The arm already declines while the mode holds (the
        // `jvmti_requires_interpreter` test above), and the latch is for
        // good: set here, it would outlive the agent's request.
        let left_for_the_interpreter = stashed.as_ref().is_some_and(|(rframe, cause, point)| {
            crate::runtime::interpreter::stashed_exit_left_for_the_interpreter(
                vm, rframe, *cause, *point,
            )
        });
        if !left_for_the_interpreter {
            site.disable_direct();
        }
        crate::runtime::interpreter::lambda_site_bump_deopted();
        if let Some((rframe, cause, point_addr)) = stashed {
            let frames_depth_on_entry = thread.frames.len();
            match crate::runtime::interpreter::resume_deopted_body_at_point(
                vm,
                thread,
                &code,
                site.cached_impl(),
                &rframe,
                cause,
                point_addr,
                frames_depth_on_entry,
                "lambda-site-direct",
            ) {
                Ok(Some(value)) => {
                    crate::runtime::interpreter::lambda_site_bump_resumed();
                    let bits = jit_abi_bits_of(value);
                    screen_const_return(&site, bits, "jit-direct-resumed");
                    // qLam: an adapted site boxes the resumed frame's value.
                    return Some(lambda_site_adapt_return(vm, thread, &site, info, bits));
                }
                Ok(None) => {
                    // No resume was possible; the owning method has been
                    // de-speculated instead. Falling through re-runs the impl
                    // FROM ENTRY, which is only legitimate when that replay is
                    // observably equivalent to the attempt that trapped.
                    //
                    // This arm used to fall through unconditionally, with a
                    // counter and a comment ("side effects and all — counted so
                    // the residual is visible rather than silent") standing in
                    // for the check. A counter makes a double-execution
                    // countable, not correct: the compiled body ran up to
                    // `rframe.bci` and committed everything before it, so for
                    // an impl that stores, does I/O, takes a monitor or calls
                    // back into Java, the re-run commits all of it a second
                    // time. That is the hibernate-reactive
                    // `reactiveRemove`-fires-twice defect named above, reached
                    // by the one path the fix for it did not cover.
                    //
                    // The interpreter's own deopt sink has asked this question
                    // for exactly this reason since the spliced-bci fix, and
                    // the two ends are supposed to "ask ONE predicate ... so
                    // they cannot drift apart again". They had drifted: this
                    // end never asked.
                    //
                    // Both halves are answerable here: the impl's bytecode is
                    // `site.cached_impl().code`, and the spliced half is the
                    // artifact's own `spliced_bodies_side_effect_free` — `code`
                    // is a `RetainedCode` keep-alive that derefs to the
                    // `CompiledMethod` this call entered.
                    crate::runtime::interpreter::lambda_site_bump_unresumable();
                    // Round 12 wave 3 (lane replay, sinks patch 2): the stash
                    // form reads the point's first-arrival bit, and answers a
                    // point the impl's artifact does not own (a nested
                    // callee's trap) with the whole-body rule instead of a bci
                    // of another method.
                    //
                    // Round 13 wave 1 (lane replay,
                    // `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`
                    // items 2 and 3): counted in the door census and refused
                    // exactly where the interpreter's one-shot door into the
                    // same impl refuses (`jit_bridge::door_rerun_or_refuse`):
                    // an own stash whose replay is unsound, of a class never
                    // redefined. A foreign stash (a nested callee's trap no
                    // call-site service claimed) and a hot-swapped impl re-run,
                    // as at that door; before, this arm alone raised
                    // `InternalError` for both.
                    let refuse = if lambda_direct_rerun_as_doors() {
                        let impl_method = site.cached_impl();
                        let (cause, exact) = crate::runtime::interpreter::door_rerun_verdict(
                            &impl_method.code,
                            &code,
                            &impl_method.class_name,
                            &impl_method.method_name,
                            &impl_method.method_descriptor,
                            Some((&rframe, point_addr)),
                        );
                        crate::runtime::interpreter::note_door_rerun(cause, exact);
                        crate::runtime::interpreter::retire_lambda_impl_after_rerun(
                            vm,
                            impl_method,
                            exact,
                        );
                        // Round 13 wave 3 (lane replay2; R13-2): the doors'
                        // one predicate, which also honours their switch
                        // `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES`. Round 13
                        // wave 8 (lane replay4): every cause, as at the doors
                        // (a foreign stash too), and a refusal is counted as
                        // one instead of as the re-run it replaced.
                        let refused = crate::runtime::interpreter::unsound_rerun_refused(
                            cause,
                            exact,
                            || class_was_redefined(vm, impl_method.declaring_class_id),
                        );
                        if refused {
                            crate::runtime::interpreter::note_door_rerun_refused(cause);
                        }
                        refused
                    } else {
                        !crate::runtime::interpreter::replay_from_entry_is_observably_equivalent_for_stash(
                            &site.cached_impl().code,
                            &code,
                            &rframe,
                            point_addr,
                        )
                    };
                    if refuse {
                        // Refuse rather than duplicate. `InternalError` is the
                        // same diagnosis the interpreter sink raises through
                        // `VmError::Internal`, delivered as a Java throwable
                        // because this path returns into compiled code and has
                        // no error channel of its own.
                        //
                        // `i64::MIN` is the return value for the same reason the
                        // `ExceptionThrown` arm below uses it:
                        // `emit_post_invoke_exception_check` compares RAX
                        // against `i64::MIN` and consults `dispatch_threw` only
                        // on that comparison, so any other word — `0`
                        // especially — leaves the exception parked and unclaimed.
                        let msg = format!(
                            "precise deoptimization unavailable for the lambda impl \
                             {}.{} at bci {}; refusing side-effecting replay",
                            site.impl_class_name(),
                            site.impl_method_name(),
                            rframe.bci,
                        );
                        // Round 12 wave 5 (lane replay3): through
                        // `throw_runtime_error`, whose heap-exhaustion rule
                        // answers with the preallocated `OutOfMemoryError`, and
                        // the singleton again for any other build failure. The
                        // old fallback returned the sentinel with NOTHING
                        // pending, which the compiled caller reads as a genuine
                        // return value (`i64::MIN`, or a bogus reference).
                        let exc = match crate::runtime::exceptions::throw_runtime_error(
                            vm,
                            thread,
                            crate::error::RuntimeError::InternalError { message: msg },
                        ) {
                            crate::error::MethodCallFailed::ExceptionThrown(exc) => Some(exc),
                            crate::error::MethodCallFailed::InternalError(_) => {
                                *vm.mem.singleton_oom.read()
                            }
                        };
                        if let Some(exc) = exc {
                            set_jit_pending_exception(thread, exc);
                        }
                        // Falling through would re-run a side-effecting body,
                        // so the sentinel is returned even in the one case left
                        // with nothing to park (no singleton yet: VM start-up).
                        return Some(i64::MIN);
                    }
                }
                Err(crate::error::MethodCallFailed::ExceptionThrown(exc)) => {
                    // Park it for the compiled caller's own post-invoke check —
                    // and return the sentinel that check actually tests for.
                    //
                    // This used to return `0`, described as "the null/zero
                    // sentinel". **There is no such sentinel.**
                    // `emit_post_invoke_exception_check` compares `RAX` against
                    // `i64::MIN` and consults `dispatch_threw` only on that
                    // comparison; `0` is a perfectly ordinary null reference
                    // return and the check keeps it. The parked exception then
                    // sat unclaimed while compiled code carried on with a null.
                    //
                    // It looked correct for a year because of what usually
                    // FOLLOWS a SAM call: unboxing the result
                    // (`Integer.intValue`) was a CALL, and that call crossed
                    // into Rust and delivered the pending exception a moment
                    // later at a site that could route it. Making the box/unbox
                    // intrinsic default-ON removed the call — the unbox became
                    // an inline load with a null-receiver guard — and the
                    // exception escaped its own `catch`. See
                    // `internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`.
                    //
                    // `i64::MIN` is right for every return type: for a
                    // reference it cannot be a valid heap address, and for the
                    // `J`/`D`/`F` shapes where it IS a representable value the
                    // check disambiguates through `dispatch_threw`, which finds
                    // exactly the signal parked on the line above.
                    set_jit_pending_exception(thread, exc);
                    crate::runtime::interpreter::lambda_site_bump_resumed();
                    return Some(i64::MIN);
                }
                Err(failed) => {
                    crate::runtime::interpreter::lambda_site_bump_unresumable();
                    // Round 12 wave 5 (lane replay3): an internal failure out
                    // of the resume (the resumed frame's own `VmError`, or a
                    // heap too full to build even the re-materialisation
                    // `OutOfMemoryError`) happened AFTER the attempt ran, so
                    // the `return None` below re-ran the impl from entry and
                    // committed its prefix again. Raised at this call instead,
                    // through the helpers' one conversion.
                    if lambda_resume_failure_raises() {
                        stash_helper_failure(
                            vm,
                            thread,
                            failed,
                            &"lambda impl resume failed",
                            true,
                            true,
                        );
                        return Some(i64::MIN);
                    }
                }
            }
        } else if lambda_direct_rerun_as_doors() {
            // The bare sentinel: the generic path re-runs the impl from entry.
            // Counted as the one-shot door counts its own (`Frameless`); round
            // 13 wave 1, lane replay. This was the one uncounted re-run left.
            let impl_method = site.cached_impl();
            let (cause, exact) = match implicit_throw_bci {
                // Round 13 wave 8 (lane replay4): an implicit exception whose
                // handler could not be resumed. The throwing instruction (an
                // array access, a division, a field access) commits nothing
                // and re-executes, so the prefix rule at its bci answers;
                // `-1` (not stamped) is the whole-body rule. Round 13 wave 12
                // (lane replay6): with the edges the entered body's local
                // handlers can take on the way to the throw.
                Some(bci) => (
                    crate::runtime::interpreter::DoorRerunCause::CalleeException,
                    crate::runtime::interpreter::replay_from_entry_is_observably_equivalent_after(
                        &impl_method.code,
                        Some(&*code),
                        u32::try_from(bci).unwrap_or(u32::MAX),
                    ),
                ),
                None => crate::runtime::interpreter::door_rerun_verdict(
                    &impl_method.code,
                    &code,
                    &impl_method.class_name,
                    &impl_method.method_name,
                    &impl_method.method_descriptor,
                    None,
                ),
            };
            crate::runtime::interpreter::note_door_rerun(cause, exact);
            crate::runtime::interpreter::retire_lambda_impl_after_rerun(vm, impl_method, exact);
            // Round 13 wave 8 (lane replay4): refused when unsound, as the
            // one-shot door's frameless arm refuses it
            // (`CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES`; an implicit
            // exception's, `CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES`).
            if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
                class_was_redefined(vm, impl_method.declaring_class_id)
            }) {
                crate::runtime::interpreter::note_door_rerun_refused(cause);
                let msg = format!(
                    "precise deoptimization unavailable for the lambda impl {}.{} after a \
                     trap with no frame; refusing side-effecting replay",
                    site.impl_class_name(),
                    site.impl_method_name(),
                );
                let exc = match crate::runtime::exceptions::throw_runtime_error(
                    vm,
                    thread,
                    crate::error::RuntimeError::InternalError { message: msg },
                ) {
                    crate::error::MethodCallFailed::ExceptionThrown(exc) => Some(exc),
                    crate::error::MethodCallFailed::InternalError(_) => {
                        *vm.mem.singleton_oom.read()
                    }
                };
                if let Some(exc) = exc {
                    set_jit_pending_exception(thread, exc);
                }
                return Some(i64::MIN);
            }
        }
        return None;
    }
    crate::runtime::interpreter::lambda_site_bump_direct();
    screen_const_return(&site, rc, "jit-direct");
    // qLam: an adapted site boxes the impl's 32-bit return.
    Some(lambda_site_adapt_return(vm, thread, &site, info, rc))
}

/// JIT r20 wave 13 (lane huntGC2): the words a lambda site's post-call routes
/// (`lambda_route_thrown_through_impl`, `lambda_direct_route_implicit`) decode
/// the impl's incoming values from, AFTER the impl ran. A site without an
/// adapter: the caller's own words, which the collector keeps current. An
/// adapted site (`CRATONVM_JIT_LAMBDA_SITE_BOXING`): the unboxed copy rebuilt
/// from those current words into `buf` -- the copy taken before the call is a
/// Rust local no collector updates, so its proxy and reference SAM words are
/// stale once a collection in the impl moved them. A wrapper is immutable,
/// so the unboxed values are the ones the impl was entered with. `None` when
/// a word no longer adapts (unreachable for words that adapted before).
///
/// SAFETY: as [`try_lambda_site_direct_call`]; `caller_args` is the compiled
/// caller's argument words for this SAM call, proxy receiver first.
unsafe fn lambda_site_args_after_call<'a>(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    caller_args: &'a [i64],
    buf: &'a mut [i64; crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS + 1],
) -> Option<&'a [i64]> {
    let Some(adapt) = site.adapt() else {
        return Some(caller_args);
    };
    let words = buf.get_mut(..caller_args.len())?;
    words.copy_from_slice(caller_args);
    for &(k, prim, wrapper) in adapt.unbox_list() {
        let raw = *caller_args.get(1 + k)?;
        if raw == 0 {
            return None;
        }
        // SAFETY: a non-zero reference word of the compiled caller is a live
        // object, as in `try_lambda_site_direct_call`.
        let obj = ObjectRef::from_raw(raw as *mut u8);
        if vm.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array
            || vm.mem.heap.class_id_of(obj) != wrapper
        {
            return None;
        }
        *words.get_mut(1 + k)? = crate::runtime::interpreter::lambda_unbox_bits(vm, prim, obj)?;
    }
    buf.get(..caller_args.len())
}

/// JIT round 20 wave 12, lane qLam (`CRATONVM_JIT_LAMBDA_SITE_BOXING`): the
/// value an adapted lambda site hands its compiled caller -- the impl's raw
/// 32-bit return boxed through the wrapper's `valueOf`, as LambdaMetafactory's
/// class boxes it. `i64::MIN` (an escaping exception is parked; a 32-bit
/// return is never that word) and every non-boxing site's bits pass through. A
/// throw out of `valueOf` itself is delivered as any helper's is.
unsafe fn lambda_site_adapt_return(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    bits: i64,
) -> i64 {
    let Some(prim) = site.adapt().and_then(|adapt| adapt.box_return()) else {
        return bits;
    };
    if bits == i64::MIN {
        return bits;
    }
    // JIT round 23 wave 3b, lane L2 (`CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX`, default
    // ON, `=0` restores the native funnel): an `Integer` box is the compiled
    // `Integer.valueOf` thin helper's own body on the thread this arm holds --
    // the box cache's objects for `-128..=IntegerCache.high`, a TLAB bump past
    // it, the canonical native only for a cold slot -- instead of a
    // `safe_native_call` of the Intrinsic per call (the caller-loader guard,
    // native state, alloc-wrapper; page
    // `r23-w2-boxing-method-ref-call-costs-microseconds-FIXED-20261021.md`). Same
    // identity rules, so the same objects -- but only where `box_primitive`
    // itself would call that Intrinsic (r23 w3 review): `lambda_box_intrinsic`
    // on, the native admitted for the compiled `valueOf` binds, and `Integer`
    // initialized. Otherwise the by-name `valueOf` (the JDK body, which also
    // initializes the class) stays authoritative, as before.
    if prim == 'I'
        && lambda_arm_tlab_box_enabled(vm)
        && vm.config.lambda_box_intrinsic
        && crate::runtime::interpreter::invoke::box_value_of_intrinsic(vm, 'I').is_some()
    {
        // Cast: JIT ABI -- the int category travels sign-extended.
        let value = i64::from(bits as i32);
        // A fresh object is parked in `native_pending_return` by the body, as
        // `lambda_box_return_bits` parks it; the helper wrapper this arm runs
        // under drops the handoff root once compiled code has the value.
        return jit_integer_value_of_direct_body_on(
            vm as *const SharedVm as i64,
            value,
            Some(&mut *thread),
        );
    }
    match crate::runtime::interpreter::lambda_box_return_bits(vm, thread, prim, bits) {
        Ok(boxed) => boxed,
        Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
    }
}

/// `CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX` (JIT round 23 wave 3b, lane L2; perf,
/// default ON, `=0` the kill switch): read once per VM
/// (`VmConfig::lambda_arm_tlab_box`). See [`lambda_site_adapt_return`].
#[inline]
fn lambda_arm_tlab_box_enabled(vm: &SharedVm) -> bool {
    vm.config.lambda_arm_tlab_box
}

/// JIT round 20 wave 12, lane qLam (`CRATONVM_JIT_LAMBDA_SITE_NATIVE`): an
/// instance site whose target is the receiver class's reviewed `Intrinsic`
/// (`lambda_native_instance_target`): the call is a compiled `invokevirtual`
/// of that method on the receiver, served by the JIT's native site cache under
/// the site's own call-site record -- the door that re-checks the registry
/// generation, redefinition, the capability gate and `--jdk-only` admission
/// on its own and declines (`None`: the general path runs the call, nothing
/// having run here) whenever it would not serve. `args_slice` is the
/// (adapted) caller words, proxy first; the impl's words are the receiver
/// (capture 0, or SAM argument 0) and the rest, as for a compiled impl.
///
/// JIT round 20 (lane fixLN): those words are a Rust local no collector
/// updates, and the door's site-cache fill can run Java (a GC point). So the
/// door is asked `WarmOnly` (it declines rather than resolve); a cold site is
/// filled with nothing but the receiver word held (`FillOnly`), and the words
/// are re-materialised from `caller_args` -- the compiled caller's own words,
/// which the collector keeps current -- before the warm call.
unsafe fn lambda_site_native_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    proxy: ObjectRef,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    caller_args: &[i64],
    native_info: &'static JitInvokeInfo,
) -> Option<i64> {
    const MAX_DIRECT_ARGS: usize = crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS;
    let sam_args = args_slice.get(1..)?;
    let total = site.total_args();
    if total > MAX_DIRECT_ARGS || site.num_captures() + sam_args.len() != total {
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    }
    if !crate::runtime::interpreter::lambda_jit_site_checkcasts_pass(vm, site, sam_args) {
        // A cast that would throw: the general path raises it.
        crate::runtime::interpreter::lambda_site_bump_refused();
        return None;
    }
    let mut words = [0i64; MAX_DIRECT_ARGS];
    crate::runtime::interpreter::lambda_jit_site_capture_args(vm, site, proxy, &mut words);
    words[site.num_captures()..total].copy_from_slice(sam_args);
    if words[0] == 0 {
        // The receiver is null (the site lookup screens it; kept local).
        return None;
    }
    let key = jit_site_key(vm.vm_identity, native_info as *const JitInvokeInfo as usize);
    let mut cold = false;
    let served = try_jit_site_cached_native_dispatch_mode(
        vm,
        Some(&mut *thread),
        native_info,
        key,
        &words[..total],
        0,
        NativeSiteDoorMode::WarmOnly(&mut cold),
    );
    let bits = match served {
        Some(bits) => bits,
        // Declined for a reason other than a cold site: the general path.
        None if !cold => return None,
        None => {
            // Fill the site holding only the receiver word, which the fill
            // reads (for its class) before any resolution and never after.
            let receiver_word = [words[0]];
            let _ = try_jit_site_cached_native_dispatch_mode(
                vm,
                None,
                native_info,
                key,
                &receiver_word,
                0,
                NativeSiteDoorMode::FillOnly,
            );
            // The words as they are NOW: the fill was a GC point.
            let mut fresh = [0i64; MAX_DIRECT_ARGS + 1];
            let now = lambda_site_args_after_call(vm, site, caller_args, &mut fresh)?;
            let sam_args = now.get(1..)?;
            let proxy_raw = *now.first()?;
            if proxy_raw == 0 || site.num_captures() + sam_args.len() != total {
                return None;
            }
            words = [0i64; MAX_DIRECT_ARGS];
            crate::runtime::interpreter::lambda_jit_site_capture_args(
                vm,
                site,
                ObjectRef::from_raw(proxy_raw as *mut u8),
                &mut words,
            );
            words[site.num_captures()..total].copy_from_slice(sam_args);
            if words[0] == 0 {
                return None;
            }
            // Still cold (the registry generation moved under the fill):
            // decline, nothing having run here.
            let mut cold = false;
            try_jit_site_cached_native_dispatch_mode(
                vm,
                Some(&mut *thread),
                native_info,
                key,
                &words[..total],
                0,
                NativeSiteDoorMode::WarmOnly(&mut cold),
            )?
        }
    };
    crate::runtime::interpreter::lambda_site_bump_direct();
    Some(lambda_site_adapt_return(vm, thread, site, info, bits))
}

/// `CRATONVM_JIT_LAMBDA_RESUME_FAILURE_RAISES`, **default ON** (`=0` restores
/// the pre-round-12-wave-5 fall-through, which re-ran the impl from entry):
/// whether `try_lambda_site_direct_call` raises an internal failure out of a
/// trapped impl's resume at the call instead of re-running the impl. Read
/// per call (j27 wave 7, lane T1IC: was a process `OnceLock`); the path is
/// cold.
fn lambda_resume_failure_raises() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LAMBDA_RESUME_FAILURE_RAISES")
}

/// `CRATONVM_JIT_LAMBDA_DIRECT_RERUN_AS_DOORS`, **default ON** (`=0` restores
/// the round-12 arm: uncounted, and `InternalError` for every unresumable
/// stash whose whole-body replay is unsound, foreign or redefined included):
/// whether `try_lambda_site_direct_call` counts its re-runs in the door census
/// and refuses only what the interpreter's one-shot door into the same impl
/// refuses (round 13 wave 1, lane replay). Not memoised: read only on a
/// deopted direct call, which latches the site off the direct arm.
fn lambda_direct_rerun_as_doors() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LAMBDA_DIRECT_RERUN_AS_DOORS")
}

/// Check a SAM call's result against the constant its impl body must return.
///
/// Off unless `CRATONVM_JIT_LAMBDA_CONST_PROBE` is set, and a no-op for the
/// overwhelming majority of sites, whose bodies are not constants. See
/// `const_int_return_of`.
fn screen_const_return(site: &crate::runtime::interpreter::LambdaJitSite, raw: i64, arm: &str) {
    if !crate::runtime::env_cache::jit_lambda_const_probe() {
        return;
    }
    let Some(expected) = site.const_return() else {
        return;
    };
    crate::runtime::interpreter::const_probe_screen(
        expected,
        raw,
        site.impl_class_name(),
        site.impl_method_name(),
        arm,
    );
}

/// Encode a resumed frame's return `Value` into the raw JIT-ABI register bit
/// pattern a compiled caller expects.
///
/// The inverse of `execute_jit_call_oneshot`'s argument decode, and the same
/// table: `void` and `null` are both the zero word, a float/double travels as
/// its bit pattern, and a reference as its raw address.
fn jit_abi_bits_of(value: Option<Value>) -> i64 {
    match value {
        Some(Value::Int(x)) => x as i64, // Cast: JIT ABI -- i64 register convention
        Some(Value::Long(x)) => x,
        Some(Value::Float(x)) => x.to_bits() as i64, // Cast: JIT ABI -- float bits to i64
        Some(Value::Double(x)) => x.to_bits() as i64, // Cast: JIT ABI -- double bits to i64
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64, // Cast: JIT ABI -- pointer to i64
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    }
}

/// The direct lambda arm's classification of an `i64::MIN` return: `true` when
/// it is NOT a deopt — an escaping throwable, a genuine `Long.MIN_VALUE`, or an
/// implicit exception the impl cannot catch — so the drained signals are put
/// back and the value handed on; `false` for a deopt, which latches the site
/// off the direct arm and resumes (or declines to re-run) the body.
///
/// The implicit clause is interpreter round i1 wave 3 (L7). A trapping stub
/// raises the deopt flag BESIDE its implicit one (`jit_throw_arithmetic`,
/// `jit_throw_aioobe`, `jit_npe_with_action`), and with no stash that pair
/// means "the body threw NPE / AIOOBE / `/ by zero`" and nothing else. Read as
/// a deopt, the signal was dropped and the generic path re-ran the impl FROM
/// ENTRY, committing every side effect before the throw a second time
/// (`tools/probes/interp/L7/L7JitDoorOutcomes.java`'s lambda, one extra
/// `sideEffects++` per site). Propagated, the compiled caller's own post-invoke
/// check builds and routes it, as for any other callee. An impl WITH an
/// exception table may catch it itself, so it answers `false` here and the
/// caller routes it through that table ([`lambda_direct_route_implicit`], wave
/// 4) before it treats anything as a deopt.
fn lambda_direct_sentinel_is_not_a_deopt(
    sig: &DrainedJitSignals,
    stashed: bool,
    impl_has_handlers: bool,
) -> bool {
    lambda_direct_sentinel_is_not_a_deopt_as(
        sig,
        stashed,
        impl_has_handlers,
        lambda_implicit_without_deopt_routes_enabled(),
    )
}

/// [`lambda_direct_sentinel_is_not_a_deopt`] with
/// `CRATONVM_JIT_LAMBDA_IMPLICIT_WITHOUT_DEOPT_ROUTES` given. Pure.
///
/// JIT round 23 wave 2, lane L3
/// (`docs/feature-designs/jdk-only-mode.md`):
/// not every implicit signal comes with the deopt flag. `jit_getfield` /
/// `jit_putfield` on a null receiver (the slow path an optimizing body's
/// implicit null check recovers into) raise the NPE flag ALONE. The old
/// expression `(implicit && !impl_has_handlers) || !sig.deopt` then read an
/// NPE out of a handler-bearing impl as "not a deopt" and restashed it for
/// the compiled caller, so it left the impl past its own `catch`
/// (`W1L3Probe.loopNpe`: `try { sink += NULL_HOLDER.x; } catch (NPE e)`,
/// called through a SAM once its C2 body superseded the C1 one). With
/// `routes` on, an implicit signal decides by the table alone: an impl with
/// handlers answers `false`, and the caller routes it through that table
/// ([`lambda_direct_route_implicit`]); the deopt flag only separates a bare
/// deopt from a genuine `Long.MIN_VALUE` when no signal is set.
fn lambda_direct_sentinel_is_not_a_deopt_as(
    sig: &DrainedJitSignals,
    stashed: bool,
    impl_has_handlers: bool,
    routes: bool,
) -> bool {
    if sig.exception.is_some() {
        return true;
    }
    if stashed {
        return false;
    }
    let implicit = sig.npe || sig.aioobe.is_some() || sig.arithmetic;
    if routes && implicit {
        return !impl_has_handlers;
    }
    (implicit && !impl_has_handlers) || !sig.deopt
}

/// `CRATONVM_JIT_LAMBDA_IMPLICIT_WITHOUT_DEOPT_ROUTES` -- correctness, default
/// ON (JIT round 23 wave 2, lane L3), `=0` restores the old classification:
/// see [`lambda_direct_sentinel_is_not_a_deopt_as`]. Read on the direct lambda
/// arm's `i64::MIN` path only.
fn lambda_implicit_without_deopt_routes_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_LAMBDA_IMPLICIT_WITHOUT_DEOPT_ROUTES",
    )
}

/// What [`lambda_direct_route_implicit`] did with an implicit exception.
enum LambdaImplicitRoute {
    /// The call is over: the impl's handler returned these result bits, or the
    /// throwable escaped the impl and is parked for the compiled caller with
    /// the `i64::MIN` sentinel its post-invoke check tests for.
    Completed(i64),
    /// Nothing was decided (no implicit signal, no table, the throwable could
    /// not be built, or the table search cannot be trusted): the caller takes
    /// its pre-existing path.
    Declined,
}

/// Route an implicit exception raised by a directly called lambda impl that
/// has its OWN exception table through that table (interpreter round i1 wave
/// 4, L7) — the direct arm's counterpart of
/// [`route_implicit_exc_through_callee`]'s serviced arm, and the same machinery:
/// [`materialize_implicit_signal`] builds the throwable, and
/// `run_jit_callee_handler` resumes the impl at the covering handler (from the
/// reason-9 frame its body published, when there is one).
///
/// `sig` is the drained signal record; it is consumed. The impl's incoming
/// arguments are re-derived from `args_slice` and the proxy's fields AFTER the
/// throwable is allocated, so a collection there cannot leave them stale — the
/// same source the generic fallback decodes from.
///
/// SAFETY: as [`try_lambda_site_direct_call`]; `args_slice` is the compiled
/// caller's argument slots for this SAM call, proxy receiver first.
unsafe fn lambda_direct_route_implicit(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    sig: DrainedJitSignals,
) -> LambdaImplicitRoute {
    let implicit = implicit_signal_of(sig.aioobe, sig.npe, sig.arithmetic);
    let cached = site.cached_impl();
    if implicit == ImplicitSignal::None
        || sig.exception.is_some()
        || cached.exception_table.is_empty()
    {
        return LambdaImplicitRoute::Declined;
    }
    // `jit_throw_aioobe` stamps its own bci; the other two do not, and the
    // search then relies on the reason-9 frame, or its pc-unknown rule.
    // Interpreter round i5 wave 102, lane L2: the impl's own stamp only.
    let athrow_bci = sig.athrow_bci_owned_by(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    );
    let throw_pc = if athrow_bci >= 0 {
        athrow_bci as usize
    } else {
        usize::MAX
    };
    let Some(exc) =
        materialize_implicit_signal(vm, thread, implicit, sig.trap_frames, sig.npe_action)
    else {
        clear_lambda_impl_exceptional_frame(vm, thread, cached);
        return LambdaImplicitRoute::Declined;
    };
    // Pinned across the argument decode and the handler search, both of which
    // can reach a safepoint (see `try_run_callee_handler`).
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let Some(args) = lambda_impl_incoming_values(vm, site, args_slice) else {
        thread.native_pin_roots.truncate(pin_base);
        clear_lambda_impl_exceptional_frame(vm, thread, cached);
        return LambdaImplicitRoute::Declined;
    };
    let exc = thread.native_pin_roots[pin_base];
    let res = crate::runtime::interpreter::run_jit_callee_handler(
        vm, thread, cached, throw_pc, exc, &args,
    );
    thread.native_pin_roots.truncate(pin_base);
    match res {
        Ok(Ok(value)) => LambdaImplicitRoute::Completed(jit_abi_bits_of(value)),
        Ok(Err(e)) => {
            LambdaImplicitRoute::Completed(handle_jit_dispatch_error(vm, thread, e, info))
        }
        // The impl's table does not cover the throw: it escapes the impl, and
        // the compiled caller's own check routes it -- an escaping exception,
        // not a deopt.
        Err((crate::runtime::interpreter::CalleeHandlerMiss::NotCaught, exc)) => {
            set_jit_pending_exception(thread, exc);
            // The impl is gone; its frame can never be claimed.
            clear_lambda_impl_exceptional_frame(vm, thread, cached);
            LambdaImplicitRoute::Completed(i64::MIN)
        }
        Err(_) => {
            clear_lambda_impl_exceptional_frame(vm, thread, cached);
            LambdaImplicitRoute::Declined
        }
    }
}

/// nio21 wave 21 L1
/// (`r21-lambda-handler-impl-sam-calls-round-trip-through-rust-FIXED-20261010.md`):
/// offer a throwable that left a handler-bearing lambda impl's compiled body
/// to THAT impl's exception table, at `throw_pc` (or the reason-9 frame its
/// body published), with the impl's incoming values (`lambda_impl_incoming_values`:
/// the proxy's captures, then the SAM arguments). The explicit-throwable twin
/// of [`lambda_direct_route_implicit`], and the same machinery
/// (`run_jit_callee_handler`) a named callee's handler runs through
/// ([`try_run_callee_handler`]).
///
/// `Ok(bits)`: the impl's handler ran and the call is over (its result, or
/// `i64::MIN` with whatever the handler's own code left pending). `Err(exc)`:
/// the table does not take it (or cannot be searched); `exc` is the throwable
/// to propagate, re-read from its pin, and the impl's stashed frame is
/// dropped. A miss is what a named callee's miss is: the throwable leaves.
///
/// SAFETY: as [`lambda_direct_route_implicit`]; `args_slice` holds the
/// compiled caller's CURRENT argument words for this SAM call, proxy first.
unsafe fn lambda_route_thrown_through_impl(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    exc: ObjectRef,
    throw_pc: usize,
) -> Result<i64, ObjectRef> {
    let cached = site.cached_impl();
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let Some(args) = lambda_impl_incoming_values(vm, site, args_slice) else {
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        clear_lambda_impl_exceptional_frame(vm, thread, cached);
        return Err(exc);
    };
    let exc = thread.native_pin_roots[pin_base];
    let res = crate::runtime::interpreter::run_jit_callee_handler(
        vm, thread, cached, throw_pc, exc, &args,
    );
    thread.native_pin_roots.truncate(pin_base);
    match res {
        Ok(Ok(value)) => Ok(jit_abi_bits_of(value)),
        Ok(Err(e)) => Ok(handle_jit_dispatch_error(vm, thread, e, info)),
        Err((_, exc)) => {
            clear_lambda_impl_exceptional_frame(vm, thread, cached);
            Err(exc)
        }
    }
}

/// nio21 wave 21 L1: the lambda site of a SAM call whose receiver is a lambda
/// proxy with a HANDLER-BEARING impl, for the callee-deopt service; `None`
/// for every other receiver (and always while
/// `CRATONVM_JIT_LAMBDA_HANDLER_IMPL` is off, which is the only way such a
/// site exists). A receiver-dispatched site only (`invoke_kind` 0 / 2).
fn lambda_handler_site_for_service(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<std::rc::Rc<crate::runtime::interpreter::LambdaJitSite>> {
    if !crate::runtime::exceptions::lambda_handler_impl_enabled()
        || !matches!(info.invoke_kind, 0 | 2)
        || receiver_class_id.as_u32() == 0
    {
        return None;
    }
    let site = crate::runtime::interpreter::lambda_jit_site(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
    )?;
    (!site.cached_impl().exception_table.is_empty()).then_some(site)
}

/// Interpreter round i5 wave 104, lane L2 (page
/// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
/// has the EXPLICIT throwable the MIC door drained already been offered to
/// the lambda impl's table, so that [`lambda_route_thrown_through_impl`] must
/// not offer it again?
///
/// * The not-entrant record names the IMPL (`jit_not_entrant_entry_body`
///   records the body it re-dispatched, while this door's `info` names the
///   SAM method, so `take_redispatch_escape_for(info)` never matched it): the
///   VM's invoke path ran the impl and searched its table at the real pc. The
///   throwable is unstamped, so the route would search with the pc unknown
///   and a typed row would match on class alone, re-running the impl's
///   handler (the `pc-unknown typed match` census lines of
///   `L2W102ForeignThrowStamp`: its shapes are method references).
///   The same rule as [`DrainedJitSignals::escaped_a_redispatch_of`].
/// * The stamp belongs to ANOTHER known method: the last compiled exit was
///   not the impl's (the proxy's own compiled body, say, whose call to the
///   impl was serviced -- and the impl's table searched -- at that inner
///   site). Its pc is no pc of the impl; offering it would range-check the
///   impl's table against a foreign bci.
///
/// An implicit signal is left to the route as before.
fn explicit_throwable_already_left_impl(
    signals: &DrainedJitSignals,
    escape_record: u64,
    site: &crate::runtime::interpreter::LambdaJitSite,
) -> bool {
    if signals.exception.is_none() {
        return false;
    }
    let imp = site.cached_impl();
    if signals.athrow_bci < 0 {
        // A record is only ever written with the switch on; asked again so
        // `=0` restores the old behaviour exactly.
        return not_entrant_escape_enabled()
            && redispatch_escape_record_names(
                escape_record,
                &imp.method_name,
                &imp.method_descriptor,
            );
    }
    // `athrow_owner` is `0` (unknown) under `CRATONVM_JIT_THROW_STAMP_OWNER=0`.
    signals.athrow_owner != 0
        && signals.athrow_owner
            != method_identity_key(&imp.class_name, &imp.method_name, &imp.method_descriptor)
}

/// The impl's incoming arguments for a direct SAM call: the proxy's captures,
/// then the SAM arguments decoded by the impl's own parameter types. `None` for
/// a shape this cannot spell (an unbound method reference, whose receiver is a
/// SAM argument rather than a capture, or an arity mismatch).
///
/// SAFETY: as [`lambda_direct_route_implicit`].
unsafe fn lambda_impl_incoming_values(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    args_slice: &[i64],
) -> Option<JitDecodedArgs> {
    let cached = site.cached_impl();
    let proxy = vm
        .mem
        .heap
        .is_object_address(*args_slice.first()? as usize)?;
    let mut values = JitDecodedArgs::new();
    for i in 0..site.num_captures() {
        values.push(vm.mem.heap.get_field(proxy, i));
    }
    // An instance impl's receiver is its first capture and is not a parameter.
    let captured_params = site
        .num_captures()
        .checked_sub(usize::from(!cached.is_static))?;
    let mut tags = DescriptorParamIter::new(&cached.method_descriptor).skip(captured_params);
    for &raw in &args_slice[1..] {
        let value = match tags.next()? {
            b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(raw as i32),
            b'J' => Value::Long(raw),
            b'F' => Value::Float(f32::from_bits(raw as u32)),
            b'D' => Value::Double(f64::from_bits(raw as u64)),
            _ if raw == 0 => Value::Object(None),
            _ => Value::Object(vm.mem.heap.is_object_address(raw as usize)),
        };
        values.push(value);
    }
    if tags.next().is_some() {
        return None;
    }
    Some(values)
}

/// Drop the reason-9 frame a lambda impl's compiled body published, if the one
/// stashed names that impl: its attempt is being abandoned, and a stale frame
/// is a later drain's mis-claim. Same rule as [`clear_own_exceptional_frame`],
/// keyed on the impl rather than on a call site's SAM name.
///
/// JIT round 17 wave 3, lane syncdoor3: the locks the dropped frame names
/// (a `synchronized` impl's handed method monitor, a block lock) are released
/// under the exceptional hand-over's switches ([`drop_exceptional_frame_of`]),
/// as [`clear_own_exceptional_frame`] releases them.
fn clear_lambda_impl_exceptional_frame(
    vm: &SharedVm,
    thread: &mut JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
) {
    if let Some((key, bci)) = cratonvm_jit::deopt::peek_exceptional_identity() {
        if exceptional_frame_key_names(&key, &cached.method_name, &cached.method_descriptor) {
            let _ = drop_exceptional_frame_of(vm, thread, &key, bci, "abandoned lambda impl");
        }
    }
}

/// Put a drained [`DrainedJitSignals`] back exactly as it was found.
///
/// The direct lambda arm has to DRAIN the signals to tell a deopt sentinel from
/// a `long` equal to `Long.MIN_VALUE`, and when the answer is "not a deopt" the
/// signals still belong to the compiled caller's own post-invoke checks.
fn restash_jit_signals(thread: &mut JvmThread, mut sig: DrainedJitSignals) {
    if let Some(exc) = sig.exception {
        set_jit_pending_exception(thread, exc);
    }
    // Same rule as `handle_compiled_callee_deopt_sentinel`'s restore: the
    // frames belong to the trap, not to this drain, so put back the ones that
    // were taken instead of sampling a fresh (shallower) stack.
    // ONE `trap_frames` slot, so exactly ONE of these three may restore it
    // and the rest come back flag-only. Writing it more than once is not a
    // tidiness point: every one of these setters ASSIGNS the slot, so a
    // second call with `None` would wipe the frames the first put back.
    // JVMS does not let one frame have two of these pending at once, so
    // the priority never arbitrates anything real -- it is written out so
    // the next reader does not have to derive that from three `take()`s.
    let frames = sig.trap_frames.take();
    if sig.npe {
        restash_jit_pending_npe(sig.npe_action, frames);
        if sig.arithmetic {
            stash_jit_pending_arithmetic();
        }
        if let Some((index, length)) = sig.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if sig.arithmetic {
        restash_jit_pending_arithmetic(frames);
        if let Some((index, length)) = sig.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if let Some((index, length)) = sig.aioobe {
        restash_jit_pending_aioobe(index, length, frames);
    }
    if sig.deopt {
        set_jit_deopt_pending();
    }
}

/// The static call site [`try_fast_lambda_int_to_double_apply`] services a
/// trapped compiled `Double.valueOf(D)` box under (round 12 wave 2,
/// `r12w2-osrfix-fast-lambda-apply-hands-up-callee-stash-patch`, item c).
static DOUBLE_VALUE_OF_BOX_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Double",
    method_name: "valueOf",
    descriptor: "(D)Ljava/lang/Double;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// SAFETY: the JIT invoke metadata and the validated proxy receiver originate
// from the active interpreted frame; callers fall back to interpretation on a miss.
unsafe fn try_fast_lambda_int_to_double_apply(
    vm: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    proxy_class_id: ClassId,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Result<Option<i64>, crate::error::MethodCallFailed> {
    if info.method_name != "apply"
        || info.descriptor != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || args.len() != 2
    {
        return Ok(None);
    }
    // Enters a compiled `get(I)D` (and `Double.valueOf`): stand down in
    // interpreter-only mode, and while a C JVMTI env lists lambda proxy
    // frames, as `try_lambda_site_direct_call` does.
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm)
        || crate::runtime::interpreter::lambda_proxy_frames_armed(vm)
    {
        return Ok(None);
    }
    let call_site = match vm.classes.lambda_call_site_for(proxy_class_id) {
        Some(call_site) => call_site,
        None => return Ok(None),
    };
    if call_site.sam_method_name.as_ref() != "apply"
        || call_site.sam_descriptor.as_ref() != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || call_site.capture_types.len() != 1
        || call_site.impl_handle.member_name.as_ref() != "get"
        || call_site.impl_handle.descriptor.as_ref() != "(I)D"
        || !matches!(
            call_site.impl_handle.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        )
    {
        return Ok(None);
    }
    let index = match vm.mem.heap.is_object_address(args[1] as usize) {
        Some(index_box) => match vm.mem.heap.get_field(index_box, 0) {
            Value::Int(index) => index,
            _ => return Ok(None),
        },
        None => return Ok(None),
    };
    let receiver = match vm.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        _ => return Ok(None),
    };
    // Class 0 for an array (`jit_dispatch_class`), which names no class, so
    // the compiled-`get` lookup below declines rather than finding the
    // COMPONENT's `get(I)D`.
    let receiver_class_id = jit_dispatch_class(vm, receiver)
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    let class_name = match vm.classes.class_manager.read().get_class(receiver_class_id) {
        Some(class) => class.name.clone(),
        None => return Ok(None),
    };
    // JVMS §5.4.6, as the peephole's `resolve_lambda_get_route` asks it: where
    // selection against the method handle's owner picks another `get(I)D` than
    // the receiver's lenient walk, the generic dispatch runs instead (round 12
    // wave 2, `r12w2-osrfix-fast-lambda-apply-hands-up-callee-stash-patch`;
    // the peephole's generic route lands here, so without this its own
    // selection test was undone).
    let selection_moves = {
        let cm = vm.classes.class_manager.read();
        cm.find_class_by_name_for_class(&call_site.impl_handle.class_name, receiver_class_id)
            .and_then(|owner| {
                crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                    cm.class_store(),
                    receiver_class_id,
                    owner,
                    "get",
                    "(I)D",
                )
            })
            .is_some()
    };
    if selection_moves {
        return Ok(None);
    }
    let compiled = {
        let cache = vm.jit.jit_cache.read();
        cache
            .get(&class_name, "get", "(I)D", receiver_class_id)
            // The `Vector.get` case again — see the sibling route above.
            .filter(|compiled| !compiled.requires_wrapped_entry)
    };
    let Some(compiled) = compiled else {
        return Ok(None);
    };
    // A compiled leaf may itself contain a safe dispatch helper. That is not a
    // reason to discard a receiver-guarded entry: JitEntryGuard covers its
    // roots and the nested helper preserves normal Java dispatch semantics.
    let jit_args = [receiver.as_ptr() as i64, index as i64];
    let vm_ptr = vm as *const _ as i64;
    // Round 12 wave 2 (lane osrfix's patch, applied by lane calls): root the
    // receiver across the call and its service (the service can re-run `get`
    // with it), and retire a stale deopt flag first, as
    // `call_compiled_entry_under_owner` does, so a flag after the call is this
    // callee's own.
    let pins = CompileArgPins::pin_raw(vm, thread, &LAMBDA_GET_DIRECT_INFO, &jit_args);
    let _ = take_jit_deopt_pending();
    let _guard = crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
        &*compiled,
        Some(thread.frames.len()),
    );
    let bits = if compiled.needs_context() {
        compiled.try_call_with_context(vm_ptr, &jit_args)
    } else {
        compiled.try_call(&jit_args)
    };
    // Service the callee's sentinel HERE, as the peephole's direct route does
    // (`lambda_get_call_then_route`): a pure deopt is resumed at its trap, then
    // the MIC door's service routes a throw through `get`'s own handler, or
    // re-runs a frameless trap or a declined stash. What it cannot service
    // keeps the sentinel, which the first arm below hands up with its signals
    // in place. `CRATONVM_JIT_LAMBDA_GET_SERVICE=0` hands it up raw, as before.
    let bits = bits.map(|bits| {
        if bits == i64::MIN && jit_dispatch_threw() != 0 && lambda_get_service_enabled() {
            resume_trapped_lambda_get(vm, thread, receiver_class_id)
                .or_else(|| {
                    handle_rust_armed_callee_sentinel(
                        vm,
                        thread,
                        &LAMBDA_GET_DIRECT_INFO,
                        receiver_class_id,
                        &jit_args,
                        Some(&pins),
                    )
                })
                .unwrap_or(i64::MIN)
        } else {
            bits
        }
    });
    pins.release(thread);
    let value = match bits {
        // r9-vmside: the compiled `get(I)D` threw or deopted. It parked the
        // signal (`jit_pending_exception`, an implicit-trap flag, the deopt
        // flag or a stashed frame) and answered with the sentinel — whose bits
        // are the double `-0.0`. This arm used to box that as
        // `Double.valueOf(-0.0)` and hand the compiled `apply` caller a real
        // object, so its post-invoke check never fired: the exception an
        // out-of-range `list.get(i)` must raise came back as `-0.0`, and the
        // parked signal was left for an unrelated later drain. Hand the
        // sentinel up instead — `apply` returns a reference, for which
        // `i64::MIN` is unambiguous, and the peek leaves every signal in place
        // for the caller's own drain. A legitimate `-0.0` sets no signal and is
        // still boxed below.
        Ok(bits) if bits == i64::MIN && jit_dispatch_threw() != 0 => return Ok(Some(i64::MIN)),
        Ok(bits) => f64::from_bits(bits as u64),
        Err(_) => return Ok(None),
    };
    let box_args = [value.to_bits() as i64];
    if let Some((_callee_pin, entry, needs_context)) =
        crate::runtime::interpreter::try_jit_compile_callee(
            vm,
            "java/lang/Double",
            "valueOf",
            "(D)Ljava/lang/Double;",
            true,
        )
    {
        if let Some(boxed) =
            try_call_compiled_entry_reentrant(entry, needs_context, vm_ptr, &box_args)
        {
            if boxed != i64::MIN || !lambda_get_service_enabled() {
                return Ok(Some(boxed));
            }
            // A trap in the compiled `valueOf` stashed its frame: service it
            // here rather than hand the stash up (osrfix's patch, item c).
            // Bootstrap class, so `try_resume_trapped_callee`'s by-name lookup
            // (declaring_class_id 0) finds it; a parked throwable (an OOM)
            // keeps the sentinel, which is unambiguous for a reference return.
            return Ok(Some(
                handle_rust_armed_callee_sentinel(
                    vm,
                    thread,
                    &DOUBLE_VALUE_OF_BOX_INFO,
                    ClassId::new(0),
                    &box_args,
                    None,
                )
                .unwrap_or(i64::MIN),
            ));
        }
    }
    let boxed = crate::vm::invoke_or_native(
        vm,
        thread,
        "java/lang/Double",
        "valueOf",
        "(D)Ljava/lang/Double;",
        &[Value::Double(value)],
    )?;
    Ok(Some(match boxed {
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => return Ok(None),
    }))
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo. args_ptr/num_args form a valid i64 slice.
// mic_ptr must point to a live JitMICSlot used for monomorphic inline cache dispatch.
// pic_ptr, when non-zero, must point to a live JitPICSlot co-allocated with the MIC at
// the same call site; the helper populates its 4-way entries via `install` so the next
// invocation hits the inline cascade emitted in `jit/src/x64.rs`.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// matching the compiled method's extern "C" calling convention.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_invoke_virtual_mic(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    // t1 wave 2, lane P: a site the compile door could not judge is judged at
    // its first compiled execution (JVMS §5.4.4; `jit_site_access_guard`).
    if !jit_site_access_repeat(vm_ptr, info_ptr) {
        if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
            jit_site_access_guard(vm_ptr, info_ptr, args_ptr, num_args)
        }) {
            return bits;
        }
    }
    // nio21 wave 15, lane L1: a statically bound site the optimizing tier gave
    // an inline cache (`CRATONVM_JIT_IR_BOUND_SITE_IC`). Never virtual
    // selection: see `jit_invoke_bound_site_mic`. Unconditional (not behind the
    // planner's switch): a kind-1 info can only reach this helper through that
    // plan, and the virtual arms below would select an override for it.
    if info_ptr != 0 && (*(info_ptr as *const JitInvokeInfo)).invoke_kind == 1 {
        return jit_invoke_bound_site_mic(vm_ptr, info_ptr, args_ptr, num_args, mic_ptr, pic_ptr);
    }
    // nio21 wave 16, lane L4 (`CRATONVM_JIT_IR_IC_PROFILE_SEED`): a starved
    // site the optimizing planner armed records the receiver this miss serves
    // ([`note_ic_profile_seed`]). One null test for every other slot. Record
    // only: a panic here must not throw into the call.
    if vm_ptr != 0 && mic_ptr != 0 && args_ptr != 0 && num_args > 0 {
        if let Some(seed) = (*(mic_ptr as *const JitMICSlot)).open_profile_seed() {
            let note = || {
                note_ic_profile_seed(vm_ptr, seed, args_ptr);
                0i64
            };
            let _ = contain("ic_profile_seed", OnPanic::RecordFor { vm_ptr }, 0, note);
        }
    }
    // Interpreter round i3 wave 81, lane L3 (`CRATONVM_REFLECT_DOOR_PHASE_STATS`,
    // a measurement instrument, default off): the reflective doors' census
    // splits its `gap` at this helper's entry and at its return (the route a
    // compiled `Method.invoke` / `Constructor.newInstance` takes). Off: one
    // registry field load per call.
    let door_census = vm_ptr != 0
        && (*(vm_ptr as *const SharedVm))
            .natives
            .native_methods
            .reflect_door_phase_stats();
    if door_census {
        cratonvm_native_builtins::reflect_door_gate_stamp(
            cratonvm_native_builtins::ReflectDoorGatePoint::HelperEntry,
        );
    }
    // `r16-ffm-plain-downcall-costs-microseconds` (FFM217-2): a bound direct
    // downcall site.
    // JIT round 20 (lane stash): both doors under the helper's panic scope.
    // JIT round 22 (lane memory, MEM20-6): asked first. Its miss is one
    // thread-local slot compare keyed by the site; the field lane below parses
    // the site descriptor and probes its own table before it can decline, and
    // that was ~3 % of every bound downcall (`perf`, `FfmHot seg`).
    // JIT round 21, lane native (FFL16-1 / FUNNEL2-19-4,
    // `CRATONVM_JIT_FFM_DIRECT_DOOR_EXACT_ONLY`): the door's table is filled
    // only for `invokeExact` sites (`try_jit_ffm_downcall_bound`'s gate), so
    // every other call skips its panic scope and table probe.
    if ffm_direct_door_may_serve(info_ptr) {
        if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
            try_ffm_direct_door(vm_ptr, info_ptr, args_ptr, num_args)
        }) {
            return bits;
        }
    }
    // 2026-10-18 (`CRATONVM_JIT_MH_REAL_FIELD_LANE`, default ON): a warm
    // real field accessor (or proven `bindTo` of one) the interpreter's
    // before-linker lanes serve, served without the native funnel. The two
    // doors are disjoint (the FFM door takes a bound downcall carrier only,
    // filed by its own bind; neither the field lane nor the lean door below
    // serves one), so their order changes no answer.
    if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
        try_jit_mh_real_field_lane(vm_ptr, info_ptr, args_ptr, num_args)
    }) {
        return bits;
    }
    // SBR5 lane ncall: a site already known to be a plain callback native.
    if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
        try_lean_native_door(vm_ptr, info_ptr, args_ptr, num_args, true)
    }) {
        // Wave 83, lane L3: a reflective door filed in the lean table
        // (`CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE`) returns here; the census's
        // `gap_out` needs this return's stamp too.
        if door_census {
            cratonvm_native_builtins::reflect_door_gate_stamp(
                cratonvm_native_builtins::ReflectDoorGatePoint::HelperExit,
            );
        }
        return bits;
    }
    let f = || jit_invoke_virtual_mic_body(vm_ptr, info_ptr, args_ptr, num_args, mic_ptr, pic_ptr);
    let bits = contain(
        "jit_invoke_virtual_mic",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    if door_census {
        cratonvm_native_builtins::reflect_door_gate_stamp(
            cratonvm_native_builtins::ReflectDoorGatePoint::HelperExit,
        );
    }
    bits
}

/// nio21 wave 15, lane L1: the inline-cache miss of a STATICALLY BOUND
/// instance site (`invoke_kind == 1`), which the optimizing tier plans with a
/// MIC/PIC pair when its direct bind found no compiled callee at compile time
/// (`cratonvm_jit` `ir_bound_site_ic_planned`, `CRATONVM_JIT_IR_BOUND_SITE_IC`;
/// `r21-unbound-static-site-is-blind-dispatch-for-the-body-life-FIXED-20261009.md`).
///
/// 1. When this thread's `DISPATCH_CACHE` already names a current compiled
///    body for the site, publish that body into the site's MIC (and PIC) under
///    the receiver's class ([`publish_bound_site_entry`]). The inline cascade
///    then CALLs it raw from the next call on, exactly as it CALLs a virtual
///    callee: no Rust frame, no quiescence pair, no frame-chain push, no
///    argument pinning, no `TLS` map probes.
/// 2. Run the call through [`jit_invoke_dispatch`], unchanged: this is the call
///    the site made before it had a cache (the blind dispatch), so every
///    semantic of the site (the NPE of a null receiver, the substituted owner,
///    natives, synchronized callees, the tier-up compile that fills
///    `DISPATCH_CACHE`, exception routing) is the old one.
///
/// Why the cache is sound for a statically bound site: its target does not
/// depend on the receiver's class at all, so keying the entry by that class
/// only narrows when it is used. Every class that reaches the site gets the
/// same body. Never published to the shared megamorphic table, whose keys are
/// VIRTUAL selection, and never through the virtual arms of
/// [`jit_invoke_virtual_mic_body`], which would select an override.
///
/// SAFETY: as [`jit_invoke_virtual_mic`]: pointers supplied by the compiled
/// caller's inline-cache slow path for the current live VM.
// JIT round 23 wave 2f (lane L1): out of line, so `jit_invoke_virtual_mic`'s frame
// (live under every virtual compiled call it serves) does not reserve this arm's
// locals.
#[inline(never)]
unsafe fn jit_invoke_bound_site_mic(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    // M8-1: an inline-cache quiescent point before any probe of ours (the
    // dispatch below asks it again; it is idempotent).
    cratonvm_jit::jit_ic_quiescent_point();
    if vm_ptr != 0 && mic_ptr != 0 && num_args > 0 && args_ptr != 0 {
        // Publication only: a panic here must not throw into the call.
        let publish = || {
            publish_bound_site_entry(vm_ptr, info_ptr, args_ptr, mic_ptr, pic_ptr);
            0i64
        };
        let _ = contain("jit_invoke_bound_site_mic", OnPanic::RecordFor { vm_ptr }, 0, publish);
    }
    jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args)
}

/// [`jit_invoke_bound_site_mic`]'s publication half. Publishes nothing unless
/// every one of these holds, so a site that cannot be published stays the
/// dispatch call it was (correct, only slower):
///
/// * the compiled-entry switches are on and nothing stands the compiled arms
///   down (a redefinition quiescence, interpreter-only mode), with the same
///   memo flushes the dispatch body runs before its `DISPATCH_CACHE` consult;
/// * the MIC's redefinition stamp is current (a moved epoch flushes the slot,
///   restamps it and publishes nothing this call, as the virtual arm does);
/// * this thread's `DISPATCH_CACHE` names a current (not superseded, not
///   retired), owned body for the site, without an `invokedynamic` trap and
///   not requiring a wrapped entry. Such a row exists only after the dispatch
///   body's loader-faithful fill entered that body for this very site;
/// * a callee with an exception table only while those are publishable at
///   all (`mic_publish_exception_table_callees`), with the site's rerun flag
///   set first, as `mark_site_rerun_pins_before_publish` does;
/// * the receiver is a plain object (an array's header id is its component's).
///
/// The receiver word is read before any safepoint of this helper, so it is the
/// address the compiled caller holds, which its own cascade just read the
/// header of.
unsafe fn publish_bound_site_entry(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) {
    if !direct_static_compiled_callee_entry_enabled()
        || !direct_virtual_compiled_callee_entry_enabled()
    {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    let mic = &*(mic_ptr as *const JitMICSlot);
    let epoch_now = vm.jit.jit_cache.redefine_epoch();
    if mic
        .redefine_epoch
        .load(std::sync::atomic::Ordering::Acquire)
        != epoch_now
    {
        if mic
            .redefine_epoch
            .swap(epoch_now, std::sync::atomic::Ordering::AcqRel)
            != epoch_now
        {
            mic.clear_compiled_entry();
            if pic_ptr != 0 {
                (*(pic_ptr as *const JitPICSlot)).clear_entries();
            }
        }
        return;
    }
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm) {
        return;
    }
    flush_class_identity_dispatch_memos(vm);
    let info_key = jit_site_key(vm.vm_identity, info_ptr as usize);
    if site_quiesced_for_redefinition(vm, info, info_key) {
        return;
    }
    flush_raw_entry_dispatch_caches();
    let found = DISPATCH_CACHE.with(|dc| {
        let dc = dc.try_borrow().ok()?;
        let c = dc.get(&info_key).filter(|c| !c.names_a_stale_body())?;
        let body = std::sync::Arc::clone(c.owner.as_ref()?.arc());
        (c.entry != 0 && !body.has_indy_trap && !body.requires_wrapped_entry).then_some((
            c.entry,
            c.needs_context,
            c.rerun_pins,
            body,
        ))
    });
    // `_callee_pin` holds the body across the publications: `update` /
    // `install` take their own keep-alive by resolving the entry, which can
    // only succeed while the artifact is alive.
    let Some((entry, needs_ctx, rerun_pins, _callee_pin)) = found else {
        return;
    };
    if rerun_pins && !mic_publish_exception_table_callees() {
        return;
    }
    let receiver_bits = *(args_ptr as *const i64) as u64;
    if receiver_bits == 0 || (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        return;
    }
    let receiver_ref = ObjectRef::from_raw(receiver_bits as usize as *mut u8);
    let Some(receiver_class) = jit_dispatch_class(vm, receiver_ref).class_id() else {
        return;
    };
    // Interpreter round i7 wave 137, lane L6 (review of wave 136 L2,
    // `CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, default OFF): this
    // publication runs BEFORE the dispatch call that makes the interface
    // sender's receiver check, from a `DISPATCH_CACHE` row an earlier PASSING
    // receiver filled. A receiver class the check refuses is never published:
    // its next call would hit the way and run the body with no
    // `IllegalAccessError`. One per-VM bool read with the switch off.
    if info.invoke_kind == 1
        && vm.config.jit_iface_special_receiver_guard
        && iface_special_receiver_refusal_message(
            vm,
            info,
            std::slice::from_raw_parts(args_ptr as *const i64, 1),
        )
        .is_some()
    {
        return;
    }
    let receiver_cid = receiver_class.as_u32();
    let class_name: String = {
        let cm = vm.classes.class_manager.read();
        match cm.get_class(receiver_class) {
            Some(class) => class.name.to_string(),
            None => return,
        }
    };
    if rerun_pins && !mic.callee_rerun_pins() {
        // Before the publication, so a reader that finds the entry finds the
        // flag (`JitMICSlot::mark_callee_rerun_pins`).
        mic.mark_callee_rerun_pins();
    }
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    mic.update(receiver_cid, &class_name, entry as u64, needs_ctx, jdk_only);
    let pic = (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot));
    if let Some(pic) = pic {
        if pic.install(receiver_cid, &class_name, entry as u64, needs_ctx, jdk_only) {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(vm, None);
        }
    }
    // The re-check AFTER the install (item 4 of
    // `docs/feature-designs/profile-guided-inlining.md`,
    // JIT r22 wave 3): a redefinition that moved the epoch between the stamp
    // compare above and the `update` / `install` withdraws what was just
    // published, so no hit can run the pre-redefinition body past the next
    // call. The stamp is read before the install and this after it.
    withdraw_ic_publication_if_redefined(vm, epoch_now, mic, pic);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] bound-site-ic published {}.{}{} for receiver class {} ({}) entry={:#x}",
            info.class_name, info.method_name, info.descriptor, receiver_cid, class_name, entry
        );
    }
}

/// nio21 wave 16, lane L4 (`CRATONVM_JIT_IR_IC_PROFILE_SEED`;
/// `r21-eagerly-compiled-callee-has-no-receiver-profile-to-splice-CLOSED-20261012.md`):
/// the inline-cache miss of a site the optimizing planner armed
/// (`cratonvm_jit::IcProfileSeedSite`: an own-site virtual call in a loop of a
/// method-entry body whose receiver profile was starved at compile time --
/// the eager callee chain compiled the method before it ever ran).
///
/// 1. The receiver's exact class, when it is new to the site, is written into
///    the method's receiver profile at the site's bci as
///    `MIN_GUARDED_RECEIVER_EVIDENCE` observations
///    (`ProfileStore::seed_ic_observed_receiver`). The miss helper sees every
///    distinct class that reaches the site (a class the MIC or PIC serves never
///    enters it), so the row is the set of classes the site has seen.
/// 2. The first such record asks for ONE optimizing recompile of the method
///    (`ProfileStore::claim_ic_seed_recompile`: once per method, a VM budget),
///    through the late-bind C2 retry door, whose publication supersedes the
///    starved body and forwards its baked callers to the new one. Only with a
///    background compiler and the C2 supersede on, as the late-bind recompile.
///
/// The recompile reads a profile with evidence at the site, so its planner
/// splices the class's body behind the exact class guard
/// (`ir::profile_guarded_receiver_class`), whose miss edge is the ordinary
/// call: no Java-visible answer can change, only which body runs inline.
///
/// SAFETY: as [`jit_invoke_virtual_mic`]: `vm_ptr` the live VM, `args_ptr` the
/// compiled caller's argument words with the receiver first.
// JIT round 23 wave 2f (lane L1): out of line, so the frame of the helper on every
// compiled-call level (which stays live for the depth of a recursion) does not reserve
// this door's locals; it returns before the call that recurses.
#[cold]
#[inline(never)]
unsafe fn note_ic_profile_seed(vm_ptr: i64, seed: &cratonvm_jit::IcProfileSeedSite, args_ptr: i64) {
    let receiver_bits = *(args_ptr as *const i64) as u64;
    if receiver_bits == 0 || (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let receiver_ref = ObjectRef::from_raw(receiver_bits as usize as *mut u8);
    // An array receiver answers no class here (its header id is its
    // component's), and class 0 is never a guard class.
    let Some(receiver_class) = jit_dispatch_class(vm, receiver_ref).class_id() else {
        return;
    };
    let receiver_cid = receiver_class.as_u32();
    if receiver_cid == 0 || !seed.note_receiver(receiver_cid) {
        return;
    }
    let key = crate::jit::profile::MethodKey {
        class_id: seed.class_id,
        method_name: std::sync::Arc::clone(&seed.method_name),
        descriptor: std::sync::Arc::clone(&seed.descriptor),
    };
    let store = &vm.jit.profile_store;
    let seeded = store.seed_ic_observed_receiver(
        &key,
        seed.pc,
        receiver_cid,
        cratonvm_jit::MIN_GUARDED_RECEIVER_EVIDENCE,
    );
    let dbg = crate::runtime::env_cache::dbg_jitc();
    if dbg {
        eprintln!(
            "[cratonvm-jitc] ir-ic-profile-seed {}.{}{} @pc={}: receiver class {} {}",
            seed.class_name,
            seed.method_name,
            seed.descriptor,
            seed.pc,
            receiver_cid,
            if seeded {
                "recorded"
            } else {
                "already in the profile"
            },
        );
    }
    // j27 HANGHUNT (`CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE`, default ON, `=0` kills): a row
    // the interpreter filled before this seed still asks; the armed seed
    // proves the body was compiled starved.
    let claim_on_enqueue = cratonvm_jit::ic_seed_claim::ic_seed_claim_on_enqueue_enabled();
    if !cratonvm_jit::ic_seed_claim::ic_seed_asks_recompile(seeded, claim_on_enqueue)
        || !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::c2_supersede()
    {
        return;
    }
    if !store.claim_ic_seed_recompile(&key) {
        return;
    }
    if dbg {
        eprintln!(
            "[cratonvm-jitc] ir-ic-profile-seed recompile requested {}.{}{} (site pc={})",
            seed.class_name, seed.method_name, seed.descriptor, seed.pc,
        );
    }
    // The C2 retry (`TieredCompilationManager::request_c2_retry`, under the
    // name "ic-profile-seed" since JIT r22 wave 2, nio21 w16 L4 P1; it was
    // the late-bind door's): one more optimizing compile, refused while one
    // is already queued or the method's retry budget is spent.
    let answer = vm.jit.tiered_manager.request_ic_seed_recompile(
        &crate::jit::tiered::MethodKey::with_class_id(
            ClassId::new(seed.class_id),
            std::sync::Arc::clone(&seed.class_name),
            std::sync::Arc::clone(&seed.method_name),
            std::sync::Arc::clone(&seed.descriptor),
        ),
    );
    // j27 HANGHUNT: a refused retry does not spend the claim (same switch).
    if !cratonvm_jit::ic_seed_claim::ic_seed_claim_kept(answer, claim_on_enqueue) {
        store.release_ic_seed_recompile(&key);
        if dbg {
            eprintln!(
                "[cratonvm-jitc] ir-ic-profile-seed claim RELEASED {}.{}{} (site pc={}): retry refused",
                seed.class_name, seed.method_name, seed.descriptor, seed.pc,
            );
        }
    }
    // JIT round 21 lane indyconst (`CRATONVM_JIT_ONE_SHOT_RETRY_OWED`): the
    // claim is spent and the site's MIC now serves this class, so nothing asks
    // again; a task holding the slot may have read the profile before the row.
    crate::runtime::interpreter::jit_bridge::owe_one_shot_retry_if_already_queued(
        vm,
        answer,
        seed.class_id,
        &seed.method_name,
        &seed.descriptor,
        "ir-ic-profile-seed",
    );
}

/// nio21 wave 16, lane L4: text pins for the inline-cache profile seed (the
/// helper cannot be driven without compiled code).
#[cfg(test)]
mod nio21w16_ic_profile_seed_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    /// The seed note runs after the bound-site route (whose slots are never
    /// armed) and before every virtual door, under a recording panic guard.
    #[test]
    fn the_seed_note_is_asked_before_the_virtual_doors() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
        let bound = body
            .find("jit_invoke_bound_site_mic(")
            .expect("the bound-site route");
        let note = body
            .find("note_ic_profile_seed(vm_ptr, seed, args_ptr)")
            .expect("the seed note");
        assert!(bound < note);
        assert!(body.contains("contain(\"ic_profile_seed\", OnPanic::RecordFor { vm_ptr }"));
        for door in [
            "try_ffm_direct_door(",
            "try_lean_native_door(",
            "jit_invoke_virtual_mic_body(",
        ] {
            assert!(note < body.find(door).unwrap(), "the note precedes {door}");
        }
    }

    /// The recompile is asked only after the profile row was written and the
    /// one-per-method claim succeeded.
    #[test]
    fn the_recompile_follows_the_record_and_the_claim() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn note_ic_profile_seed(");
        let record = body.find("seed_ic_observed_receiver(").expect("the record");
        let claim = body.find("claim_ic_seed_recompile(").expect("the claim");
        let ask = body
            .find("request_ic_seed_recompile(")
            .expect("the request");
        assert!(record < claim && claim < ask);
    }

    /// j27 HANGHUNT (`CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE`): a filled row
    /// still asks under the switch, and a refused retry hands the claim back,
    /// after the request answered.
    #[test]
    fn j27_hanghunt_a_refused_seed_retry_releases_its_claim() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn note_ic_profile_seed(");
        assert!(body.contains("ic_seed_asks_recompile(seeded, claim_on_enqueue)"));
        let ask = body.find("request_ic_seed_recompile(").expect("the request");
        let kept = body
            .find("ic_seed_claim_kept(answer, claim_on_enqueue)")
            .expect("the claim verdict");
        let release = body.find("release_ic_seed_recompile(").expect("the release");
        assert!(ask < kept && kept < release);
    }
}

/// nio21 wave 15, lane L1: text pins for the bound-site inline cache (the
/// helper cannot be driven without compiled code).
#[cfg(test)]
mod nio21w15_bound_site_ic_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    /// A kind-1 info is routed before every virtual door of the MIC helper:
    /// none of them may select an override for a statically bound site.
    #[test]
    fn a_bound_site_leaves_the_virtual_mic_helper_first() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
        let route = body
            .find(
                "jit_invoke_bound_site_mic(vm_ptr, info_ptr, args_ptr, num_args, mic_ptr, pic_ptr)",
            )
            .expect("the bound-site route");
        for door in [
            "try_ffm_direct_door(",
            "try_lean_native_door(",
            "jit_invoke_virtual_mic_body(",
        ] {
            let at = body.find(door).unwrap_or_else(|| panic!("{door} is asked"));
            assert!(route < at, "the bound-site route precedes {door}");
        }
    }

    /// The call itself is the blind dispatch's, and the publication never
    /// reaches the shared megamorphic table (keyed by virtual selection).
    #[test]
    fn the_bound_site_arm_calls_the_dispatch_door_and_publishes_locally() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let arm = body_of(&src, "\nunsafe fn jit_invoke_bound_site_mic(");
        assert!(arm.contains("jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args)"));
        let publish = body_of(&src, "\nunsafe fn publish_bound_site_entry(");
        for banned in [
            "publish_mega_dispatch_entry(",
            "bind_mega_selector_at_resolution(",
        ] {
            assert!(
                !publish.contains(banned),
                "the bound-site publication calls {banned}"
            );
        }
        // Every gate runs before the first publication.
        let update = publish.find("mic.update(").expect("the MIC publication");
        for gate in [
            "jvmti_requires_interpreter(vm)",
            "site_quiesced_for_redefinition(vm, info, info_key)",
            "names_a_stale_body()",
            "!body.has_indy_trap",
            "!body.requires_wrapped_entry",
            "mic_publish_exception_table_callees()",
            "mic.mark_callee_rerun_pins()",
            // Interpreter round i7 wave 137, lane L6: a receiver class the
            // interface-sender receiver check refuses is never published.
            "iface_special_receiver_refusal_message(",
        ] {
            let at = publish
                .find(gate)
                .unwrap_or_else(|| panic!("missing gate {gate}"));
            assert!(at < update, "{gate} runs before the publication");
        }
    }

    /// Interpreter round i7 wave 138, lane L2: the lean native door answers
    /// ahead of the interface-sender receiver guard in `jit_invoke_dispatch`
    /// and its kind-1 records carry no receiver guard, so a guarded site is
    /// never filed in its table.
    #[test]
    fn the_lean_native_door_never_files_a_guarded_interface_special_site() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let door = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_dispatch(");
        assert!(door.contains("try_lean_native_door("));
        let fill = body_of(&src, "\nfn lean_native_site_fill(");
        let gate = fill
            .find("iface_special_site_is_guarded(vm, info)")
            .expect("the guarded-site gate");
        let file = fill.find("lean_native_site_file(").expect("the filing");
        assert!(gate < file, "the gate runs before the filing");
    }
}

/// JIT round 19 (lane mega19, MEGA5-18-4,
/// `CRATONVM_JIT_MEGA_GATE_FIRST_NO_INLINE_PUBLISH`, default OFF): whether the
/// resolving helper leaves the site's MIC alone because the site's code tests
/// its (set) megamorphic gate before the MIC and every inline way
/// (`JitPICSlot::skips_inline_publication`; the PIC's own `install` skips its
/// inline ways on the same answer). A skipped publication only leaves a cache
/// cold; the helper's own MIC fast path then misses to its full resolution.
///
/// # Safety
///
/// `pic_ptr` is 0 or the live `JitPICSlot` the calling site passed.
unsafe fn mic_publication_skipped(pic_ptr: i64) -> bool {
    pic_ptr != 0 && (*(pic_ptr as *const JitPICSlot)).skips_inline_publication()
}

/// Round 13 (lane crash): is this thread's native stack at or below the
/// compiled-recursion floor (`compute_self_call_stack_floor`, the value the
/// self-call guard and the frame-cached `CMP RSP, [rbp - floor]` use)?
///
/// The resolving dispatch helpers ask it before calling a compiled callee, so
/// a recursion closed through inline-cache hits -- which the per-thread
/// dispatch DEPTH never counts -- still ends in a catchable
/// `StackOverflowError` rather than a SIGSEGV on the guard page.
/// `CRATONVM_JIT_IC_STACK_GUARD=0` answers `false`; the flag is read only
/// below the floor, so the common path is one TLS load and a compare.
fn jit_native_stack_exhausted() -> bool {
    let probe = 0u8;
    let sp_now = &probe as *const u8 as usize;
    let floor = JIT_SELF_CALL_STACK_FLOOR.with(|f| {
        let v = f.get();
        if v != usize::MAX {
            v
        } else {
            let computed = compute_self_call_stack_floor(sp_now, current_self_call_budget());
            f.set(computed);
            computed
        }
    });
    sp_now <= floor && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_STACK_GUARD")
}

/// `CRATONVM_JIT_ARRAY_CLONE_DIRECT` (strings round, `str16`; **default ON**,
/// `=0` is the kill switch): [`jit_invoke_virtual_mic_body`] answers an array
/// receiver's `clone()` with `Object.clone`'s registered native directly,
/// instead of the MIC resolution and the by-name `invoke_or_native` walk. Read
/// per call of that shape only (a name compare first), no static.
fn jit_array_clone_direct_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ARRAY_CLONE_DIRECT")
}

/// gch h1/fr (2026-10-08, `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`):
/// `[MIC_TRACE] <callee> kind=<k> cycles=<n>`, printed when the
/// [`jit_invoke_virtual_mic`] call it guards returns, under
/// `CRATONVM_DBG_MIC_TRACE` (the switch of `jit_invoke_dispatch`'s
/// `[DISP_TRACE]`). vg5's trace showed only `[DISP_TRACE]` lines because this
/// door printed none, so a call served here could not be told apart from one
/// no Rust door served at all (a machine-code inline-cache hit or a direct
/// call). Off, it is one cached load per call.
struct MicDoorTrace {
    label: String,
    t0: u64,
}

impl MicDoorTrace {
    #[inline]
    fn new(info: &JitInvokeInfo) -> Option<Self> {
        if !mic_prof::trace_enabled() {
            return None;
        }
        Some(Self {
            label: mic_door_trace_label(info),
            t0: mic_prof::now(),
        })
    }
}

impl Drop for MicDoorTrace {
    fn drop(&mut self) {
        eprintln!(
            "[MIC_TRACE] {} cycles={}",
            self.label,
            mic_prof::now().wrapping_sub(self.t0)
        );
    }
}

/// The callee part of a `[MIC_TRACE]` line: `[DISP_TRACE]`'s format.
fn mic_door_trace_label(info: &JitInvokeInfo) -> String {
    format!(
        "{}.{}{} kind={}",
        info.class_name, info.method_name, info.descriptor, info.invoke_kind
    )
}

#[cfg(test)]
mod gch_h1fr_mic_trace_tests {
    use super::{mic_door_trace_label, JitInvokeInfo};

    /// gch h1/fr: the MIC door's trace names its callee as `[DISP_TRACE]` does.
    #[test]
    fn gch_h1fr_mic_trace_label_matches_the_dispatch_trace() {
        let info = JitInvokeInfo {
            class_name: "Gcd1ArgPinSpecialProbe$Leaf",
            method_name: "dropVirtual",
            descriptor: "([JZ)Z",
            num_jit_args: 3,
            return_type: b'Z',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        assert_eq!(
            mic_door_trace_label(&info),
            "Gcd1ArgPinSpecialProbe$Leaf.dropVirtual([JZ)Z kind=0"
        );
    }
}

/// `CRATONVM_JIT_MH_REAL_FIELD_LANE` (performance, **default ON**, `=0` is
/// the kill switch; 2026-10-18, page
/// `docs/feature-designs/jdk-only-mode.md`).
/// Read once per process.
fn jit_mh_real_field_lane_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0);
    match STATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on =
                cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_REAL_FIELD_LANE");
            STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Lines of [`MH_REAL_FIELD_LANE_NEG`] (a power of two, direct-mapped).
const MH_REAL_FIELD_LANE_NEG_SLOTS: usize = 8;

thread_local! {
    /// Handles [`try_jit_mh_real_field_lane`] can never serve, by (VM
    /// identity, handle word, address epoch): `jit_real_field_lane_never_serves`
    /// said so for that very handle in that epoch, and its answer does not
    /// change for the handle's life. Filed only when the handle's word needed
    /// no forwarding, so the key is the word compiled code passes.
    static MH_REAL_FIELD_LANE_NEG: std::cell::Cell<[(usize, usize, u64); MH_REAL_FIELD_LANE_NEG_SLOTS]> =
        const { std::cell::Cell::new([(0, 0, 0); MH_REAL_FIELD_LANE_NEG_SLOTS]) };
}

/// A compiled `MethodHandle.invokeExact` / `invoke` site whose handle is a
/// warm real field accessor (or a proven `bindTo` of one), served here
/// without the native funnel: `None` declines with nothing disturbed, and the
/// helper then routes the call as before.
///
/// Before this, such a call reached the same field access through the whole
/// compiled-to-native route: the constant-bind and compiled-link redirects'
/// probes (asked and declined on every call), the site-cached native door's
/// `VarHandle` / byte-view / segment probes, the argument decode, the
/// `poly_call_site` arm, the full `safe_native_call_*` funnel, the `invoke` /
/// `invokeExact` native's own lane order, then the poly return's unbox. That
/// made a warm getter cost ~0.9 us with the JIT against ~0.5 us interpreted
/// (`L4W95FieldHandleWarm`), and a compiled `()I` bound getter ~3.6 us, where
/// the interpreter's lane answered in ~0.7 us (HotSpot: 10-25 ns).
///
/// What is served is exactly what the natives' before-linker lanes serve,
/// through their own guards (`lang_invoke::jit_real_field_lane_warm`), and only
/// when everything those lanes need is already on file: no Java, no frame
/// walk, no allocation, no GC point. So the call's Java-visible behaviour is
/// the interpreter's lane's, as it already was for an interpreted caller. The
/// arguments are forwarded and heap-validated first, as the helper body does
/// before its own native door; a reference answer goes back raw, with no
/// handoff root, since no GC point follows the read. A getter whose answer is
/// not the site's own variant declines after the read (a read has no
/// effect).
///
/// SAFETY: as [`jit_invoke_virtual_mic`]: pointers supplied by the compiled
/// caller for the current live VM.
// JIT round 23 wave 2f (lane L1): out of line, so the frame of the helper on every
// compiled-call level (which stays live for the depth of a recursion) does not reserve
// this door's locals; it returns before the call that recurses.
#[inline(never)]
unsafe fn try_jit_mh_real_field_lane(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    if vm_ptr == 0 || info_ptr == 0 || args_ptr == 0 || !(1..=3).contains(&num_args) {
        return None;
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    if info.invoke_kind != 0
        || info.declaring_class_id == 0
        || !matches!(info.method_name, "invokeExact" | "invoke")
        || info.class_name != "java/lang/invoke/MethodHandle"
        || !jit_mh_real_field_lane_enabled()
    {
        return None;
    }
    // The site's parameter kinds, one word each, after the handle.
    let mut kinds = [0u8; 2];
    let mut count = 0usize;
    let d = info.descriptor.as_bytes();
    if d.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    while *d.get(i)? != b')' {
        let kind = d[i];
        while d.get(i) == Some(&b'[') {
            i += 1;
        }
        if *d.get(i)? == b'L' {
            i += d[i..].iter().position(|&b| b == b';')?;
        }
        i += 1;
        if count >= kinds.len() {
            return None;
        }
        kinds[count] = kind;
        count += 1;
    }
    if count + 1 != num_args as usize {
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // A handle this lane can never serve (a method handle, an adapter) was
    // remembered by its word: decline before any heap work. The key is only
    // compared, never dereferenced, and a moved handle just misses.
    let neg_key = (
        vm.vm_identity,
        *(args_ptr as *const i64) as usize,
        vm.mem.address_epoch(),
    );
    let neg_slot = (neg_key.1 >> 3) & (MH_REAL_FIELD_LANE_NEG_SLOTS - 1);
    if MH_REAL_FIELD_LANE_NEG.with(|n| n.get()[neg_slot] == neg_key) {
        return None;
    }
    // The helper body's own prologue before it reads a word of the heap.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let raw_args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let (forwarded, validated) = forward_jit_reference_args_validated(vm, info, raw_args);
    let args = forwarded.as_deref().unwrap_or(raw_args);
    let handle = prevalidated_jit_ref(validated, 0, args[0])?;
    let mut extra = [Value::Object(None); 2];
    for i in 0..count {
        let raw = args[i + 1];
        extra[i] = match kinds[i] {
            b'I' | b'Z' | b'B' | b'S' | b'C' => Value::Int(raw as i32),
            b'J' => Value::Long(raw),
            b'F' => Value::Float(f32::from_bits(raw as u32)),
            b'D' => Value::Double(f64::from_bits(raw as u64)),
            b'L' | b'[' if raw == 0 => Value::Object(None),
            b'L' | b'[' => Value::Object(Some(prevalidated_jit_ref(validated, i + 1, raw)?)),
            _ => return None,
        };
    }
    let (thread, _jit_thread_guard) = jit_thread_mut()?;
    let answer = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let served = cratonvm_native_builtins::lang_invoke::jit_real_field_lane_warm(
            &mut ctx,
            handle,
            &extra[..count],
            info.descriptor,
            info.declaring_class_id,
        );
        match served {
            Some(answer) => answer,
            None => {
                if forwarded.is_none()
                    && cratonvm_native_builtins::lang_invoke::jit_real_field_lane_never_serves(
                        &mut ctx, handle,
                    )
                {
                    MH_REAL_FIELD_LANE_NEG.with(|n| {
                        let mut lines = n.get();
                        lines[neg_slot] = neg_key;
                        n.set(lines);
                    });
                }
                return None;
            }
        }
    };
    let value = match answer {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match (info.return_type, value) {
        (b'V', _) => 0,
        (b'I' | b'Z' | b'B' | b'S' | b'C', Some(Value::Int(v))) => v as i64,
        (b'J', Some(Value::Long(v))) => v,
        (b'F', Some(Value::Float(v))) => v.to_bits() as i64,
        (b'D', Some(Value::Double(v))) => v.to_bits() as i64,
        (b'L' | b'[', Some(Value::Object(None))) => 0,
        // No handoff root (`native_pending_return`): nothing between the
        // field read and this return is a GC point, and a root left set here
        // would outlive the hand-off (handoff-w36b).
        (b'L' | b'[', Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        // Not the site's own variant: a getter's read had no effect, so the
        // native route answers the call instead.
        _ => return None,
    })
}

/// JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`, page
/// `docs/known-issues/jit/jr21-dispatch-five-class-site-goes-megamorphic-20261014.md`):
/// may [`jit_invoke_virtual_mic_body`] leave `receiver_cid` out of the site's
/// inline ways? Only at a MIC-first optimizing-tier site whose MIC now holds
/// that class with an entry, and only where the MIC is still published (a
/// gate-first megamorphic site never reads it, `skips_inline_publication`).
/// The class is then served by the MIC inline, by the shared megamorphic
/// table (`publish_mega_dispatch_entry`, still called) and by the helper;
/// never a wrong body: a skipped install publishes nothing.
fn pic_way_redundant_with_mic(mic: &JitMICSlot, pic: &JitPICSlot, receiver_cid: u32) -> bool {
    !pic.skips_inline_publication() && pic.mic_serves_class_inline(mic, receiver_cid)
}

/// Body of [`jit_invoke_virtual_mic`], run under its panic guard.
unsafe fn jit_invoke_virtual_mic_body(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    // JIT round 14 wave 2 (lane mic, M8-1): past the probe that missed, before
    // any probe of ours -- an inline-cache quiescent point. Must stay first.
    cratonvm_jit::jit_ic_quiescent_point();
    // JIT round 16 (lane mh, MH15-5): see `mh_constant_bind_redirect`.
    let mut mh_link_threw = false;
    if let Some(route) =
        mh_constant_bind_redirect(vm_ptr, info_ptr, args_ptr, num_args, &mut mh_link_threw)
    {
        return mh_constant_bind_call(vm_ptr, route, args_ptr, num_args);
    }
    if mh_link_threw {
        // Round 18 wave 11 (lane mhresid): the site's link stashed its error.
        return i64::MIN;
    }
    // JIT round 20 wave 19, lane pRefl (`CRATONVM_JIT_MH_COMPILED_LINK`): a
    // linked `invokeExact`, an `invokeBasic` or a `linkTo*` calls its Java
    // target as a compiled call, not through its native door.
    if let Some(bits) = mh_compiled_link_redirect(vm_ptr, info_ptr, args_ptr, num_args) {
        return bits;
    }
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    mic_prof::dump_maybe();
    let _cyc_total = mic_prof::CycGuard::new(&mic_prof::CYC_MIC_TOTAL);
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    // gch h1/fr: this door's per-call line, on `[DISP_TRACE]`'s switch.
    let _mic_trace = MicDoorTrace::new(info);
    let _callee_guard = if crate::runtime::env_cache::jit_putfield_diag() {
        Some(JitCalleeGuard::new(info))
    } else {
        None
    };
    let cv_trace = cv_trace_match(info);
    if cv_trace {
        let mut bits = [0i64; 4];
        if num_args > 0 && !(args_ptr as *const i64).is_null() {
            for (i, b) in bits.iter_mut().enumerate().take((num_args as usize).min(4)) {
                *b = *(args_ptr as *const i64).add(i);
            }
        }
        eprintln!(
            "[cv-mic-entry] cp_class={} num_args={} a0={:#x} a1={:#x}",
            info.class_name, num_args, bits[0], bits[1]
        );
    }
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        if cv_trace {
            eprintln!("[cv-mic-earlyout] bad num_args/args_ptr -> silent 0");
        }
        return 0;
    }
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    // BUG-1: native-stack recursion guard for the JIT→JIT virtual dispatch
    // path (the `invokevirtual` sibling of `jit_invoke_dispatch`). Same
    // contract: bump the per-thread JIT-dispatch depth, and on overflow stash
    // a catchable `StackOverflowError` + return the `i64::MIN` deopt sentinel
    // rather than recursing into a compiled callee and blowing the OS stack.
    // Held for the rest of the call so the level releases on every return.
    let _jit_dispatch_depth_guard =
        match enter_jit_dispatch(vm, constructor_receiver(info, args_slice)) {
            Ok(g) => g,
            Err(sentinel) => return sentinel,
        };
    // Round 13 (lane crash): the native-stack floor check for a recursion
    // whose other levels are raw inline-cache hits now runs inside
    // `enter_jit_dispatch` above (round 13 wave 2, lane mega: patch 2), which
    // the inline MIC/PIC guard's low-stack edge reaches through this helper.

    let (thread, _jit_thread_guard) = match jit_thread_mut() {
        Some(t) => t,
        None => {
            if cv_trace {
                eprintln!("[cv-mic-earlyout] no jit thread -> silent 0");
            }
            return 0;
        }
    };

    if args_slice.is_empty() {
        if cv_trace {
            eprintln!("[cv-mic-earlyout] empty args -> silent 0");
        }
        return 0;
    }
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
        && null_resource_name_receiver_is_class_loader(
            vm,
            args_slice[0],
            info.method_name,
            info.descriptor,
        )
    {
        set_jit_pending_npe();
        return i64::MIN;
    }
    let receiver_raw = args_slice[0];
    if receiver_raw == 0 {
        // Null receiver: throw NullPointerException (JVM semantics), mirroring
        // the `jit_invoke_dispatch` fix. Set the pending-NPE flag + return the
        // i64::MIN sentinel so the interpreter's post-JIT drain builds the real
        // NPE and routes it through the method's exception table. Was `return
        // 0`, which silently swallowed a null-receiver call in hot JIT'd code.
        //
        // `INVOKE_RECEIVER`, and not the bare setter, for the reason the
        // dispatch-helper twin gives: the code names the opcode family that
        // trapped so `runtime::interpreter::jit_npe_message` can corroborate
        // the bci it recovers from this frame's safepoint-id slot. The two
        // doors must agree — a `getMessage()` that depended on whether the call
        // site had warmed into a monomorphic inline cache would be its own
        // wrong answer.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // Defensive: a receiver slot carrying tagged-long bits (low 3 bits set
    // or value above the 48-bit canonical-address ceiling) is not a valid
    // heap pointer.  Bail out with rc=0 (the dispatcher's "no result"
    // path); this mirrors the receiver_raw == 0 short-circuit above and
    // avoids the `ObjectRef::from_raw` alignment panic.
    let receiver_bits = receiver_raw as u64;
    if (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        if cv_trace {
            eprintln!(
                "[cv-mic-earlyout] misaligned/tagged receiver {:#x} -> silent 0",
                receiver_bits
            );
        }
        return 0;
    }
    // SAFETY: receiver_bits is non-zero, 8-byte aligned, and within the
    // 48-bit canonical address space — matches the invariants required by
    // ObjectRef::from_raw for live heap objects.
    // The SATB flush above may have moved either the receiver or a reference
    // parameter.  A MIC hit jumps straight into compiled code with this raw
    // slice, so canonicalize every reference before the class lookup and the
    // eventual direct call.
    //
    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`, JIT round 19 wave 2, lane
    // funnel2): see the sibling in `jit_invoke_dispatch_body`. Between here
    // and the native-site door below only the array-`clone` arm can run
    // anything, and it returns when it does.
    let (forwarded_args, prevalidated) = if vm.natives.native_methods.jit_native_walk_once() {
        forward_jit_reference_args_validated(vm, info, args_slice)
    } else {
        (forward_jit_reference_args(vm, info, args_slice), 0)
    };
    let args_slice = forwarded_args.as_deref().unwrap_or(args_slice);
    // jr20 wave 9, lane gcE (gch-h1jp item 2): a forwarded Rust copy of the
    // arguments is in no root set, so every arm below that calls a compiled
    // body with it roots its words across the call and the sentinel service
    // (`CompileArgPins`, read back through `args_now`): a re-run from entry
    // after a moving collection inside the callee must not read the copy's
    // pre-move words. Rare (ZGC's lazy forwarding), so the cost is too.
    let args_forwarded = forwarded_args.is_some();
    let receiver_raw = args_slice[0];
    let receiver_ref = ObjectRef::from_raw(receiver_raw as usize as *mut u8);

    // Strings round (`str16`): an ARRAY receiver's `clone()`. JDK 25
    // `Arrays.copyOf` / `copyOfRange` return `original.clone()` for a
    // whole-array copy (every `StringBuilder.toString()` of a builder whose
    // `value` is exactly full), and the site cache below declines every array
    // receiver (its class-id key would alias the component's class), so each
    // call took the whole MIC resolution and a by-name `invoke_or_native` walk
    // to reach `Object.clone`'s registered native. Call that native here,
    // resolved and admitted by the same policy test the leaf-native doors use.
    if info.method_name == "clone"
        && info.descriptor == "()Ljava/lang/Object;"
        && args_slice.len() == 1
        && jit_array_clone_direct_enabled()
        && matches!(
            jit_dispatch_class(vm, receiver_ref),
            JitDispatchClass::JavaLangObject
        )
    {
        let clone_native = vm
            .natives
            .native_methods
            .resolve_id("java/lang/Object", "clone", "()Ljava/lang/Object;")
            .and_then(|id| {
                let callback = vm.natives.native_methods.callback_of(id)?;
                admit_jit_fast_native_resolved(
                    vm,
                    "java/lang/Object",
                    "clone",
                    "()Ljava/lang/Object;",
                    callback,
                    Some(id),
                )
            });
        if let Some((callback, native_id)) = clone_native {
            count_jit_native_dispatch(vm, native_id);
            let values = [Value::Object(Some(receiver_ref))];
            return match crate::vm::safe_native_call(vm, thread, callback, &values) {
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // Leaf-native fast path — the invokevirtual/invokeinterface half.
    //
    // This entry point, not `jit_invoke_dispatch`, is where compiled virtual
    // calls arrive: the x64 backend emits `jit_invoke_virtual_mic` for them and
    // reserves the generic dispatcher for static/special sites and bailouts.
    // Hooking only the dispatcher left every *instance* leaf native — the whole
    // `AtomicInteger`/`AtomicLong` accessor set, i.e. item 2 of the AQS doc —
    // still paying the full funnel, with the site never even reaching
    // `resolve_native_site` to be counted as a refusal. `AtomicInteger.get`
    // measured 1026 ns before and 926 ns after the dispatcher-only version:
    // unchanged, while `Math.abs` (static, same mechanism) went 330 -> 84.
    //
    // Placed immediately after `forward_jit_reference_args` so the receiver this
    // reads is the post-SATB-flush address, and before the MIC/PIC machinery,
    // which a native leaf has no use for — there is no compiled callee to cache.
    //
    // `NATIVE_SITE_CACHE` is keyed on `(vm_identity, info_ptr)`, and a
    // `JitInvokeInfo` box dies with its `CompiledMethod`, so the key can already
    // name a different call site. Revalidate here rather than at the MIC/PIC
    // block further down, which this path returns before ever reaching. See
    // `flush_raw_entry_dispatch_caches`.
    flush_raw_entry_dispatch_caches();
    // Diagnostic (`CRATONVM_DBG_SITE_ALIAS`): does this key still name its site?
    // The gate is here rather than inside, for the same reason as the sibling
    // call site above: an all-off run must not make the call at all. This was
    // the ONE of the three call sites that had the comment but not the `if`, so
    // the "diagnostic" printed `[site-alias]` lines on stderr and grew the
    // unbounded `SITE_IDENTITY` map on every raw-entry native dispatch in an
    // ordinary run — caught by `scripts/jdk-only-strict-probes.sh`, which saw
    // the lines in BOTH CratonVM arms of `JdkOnlyPlatformProbe`.
    if site_alias_detect_enabled() {
        note_site_identity(jit_site_key(vm.vm_identity, info_ptr as usize), info);
    }
    if let Some(result) = try_jit_site_cached_native_dispatch(
        vm,
        Some(thread),
        info,
        jit_site_key(vm.vm_identity, info_ptr as usize),
        args_slice,
        prevalidated,
    ) {
        disp_census::note(disp_census::MIC_SITE_NATIVE);
        return result;
    }
    // Round 15 wave 3 (lane vh3): an invoke-exact `VarHandle` the site cache
    // declined is judged here, as `jit_invoke_dispatch`'s tail does; every arm
    // below ends in `invoke_or_native`, which cannot. A name test, then one
    // relaxed load until the VM mints an exact handle.
    if crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(info.class_name) {
        if let Some(result) = jit_mic_var_handle_exact_refusal(vm, &mut *thread, info, args_slice) {
            return result;
        }
    }

    // WS1 (kafka JIT throughput): the `Value` decode is deferred. The MIC-hit
    // fast path dispatches straight off the raw `args_slice` and never
    // materializes `Value`s; only the lambda, register-overflow-bailout and
    // full-resolution paths pay for the decode. Eagerly building this Vec
    // (one heap alloc + a descriptor parse) on every call — including pure
    // cache hits — was a measured contributor to JIT'd call-heavy code
    // running slower than the interpreter.
    let decode_values = || -> JitDecodedArgs {
        let mut values = JitDecodedArgs::with_capacity(args_slice.len());
        values.push(Value::Object(Some(receiver_ref)));
        let mut desc_iter = DescriptorParamIter::new(info.descriptor);
        for &raw in &args_slice[1..] {
            let val = match desc_iter.next() {
                Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                    Value::Int(raw as i32)
                }
                Some(b'J') => Value::Long(raw),
                Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
                Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
                Some(b'L') | Some(b'[') => {
                    if raw == 0 {
                        Value::Object(None)
                    } else {
                        // Tagged-long bits leaked into an L/[ slot (e.g. a
                        // primitive that should have been boxed before this
                        // virtual call) are downgraded to null instead of
                        // being treated as a heap pointer. The alignment +
                        // 48-bit-ceiling check alone is NOT sufficient: a
                        // round-number primitive long (e.g. a millisecond
                        // timeout like 60000/120000) is also 8-byte-aligned
                        // and well under 2^48, so it silently passed as a
                        // "plausible" pointer here and `ObjectRef::from_raw`
                        // fabricated a bogus reference into unmapped memory --
                        // confirmed via a live gdb repro: `XnioWorker$Builder
                        // .set(Option, Object)` receiving an unboxed 120000
                        // (0x1d4c0) segfaulted inside `read_string` when the
                        // bogus ObjectRef's header was dereferenced. Match
                        // `decode_dispatch_values`'s sibling path (used by the
                        // cache-miss/bailout decode) and require actual heap
                        // membership, not just bit-pattern plausibility.
                        let bits = raw as u64;
                        let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                            vm.mem.heap.is_object_address(bits as usize)
                        } else {
                            None
                        };
                        if cv_trace && validated.is_none() {
                            eprintln!(
                                "[cv-mic-decode] arg bits {:#x} FAILED is_object_address -> downgraded to null",
                                bits
                            );
                        }
                        match validated {
                            Some(obj) => Value::Object(Some(obj)),
                            None => Value::Object(None),
                        }
                    }
                }
                _ => Value::Int(raw as i32),
            };
            values.push(val);
        }
        values
    };

    // Interpreter round i3 wave 81, lane L3 (`CRATONVM_JIT_REFLECT_DOOR_DIRECT`,
    // per VM, default OFF): a compiled `Method.invoke` /
    // `Constructor.newInstance` call. Both doors are served by a forced
    // native in either mode (the interpreter's cached door,
    // `native_override_for_cached_reflect_invoke`), and the native site cache
    // above refuses the `invoke` name (`site_name_is_special_cased`), so
    // without this arm every call ran the MIC resolution, the compile probe
    // and the by-name `invoke_or_native` cascade below to reach the same
    // callback. The site's owner pins the receiver: both classes are final
    // and declare the method, so the call-site triple is the receiver's.
    // Off: one registry field load per helper call that gets here.
    if vm.natives.native_methods.jit_reflect_door_direct() {
        if let Some(callback) = crate::runtime::interpreter::jit_forced_reflect_door_native(
            vm,
            info.class_name,
            info.method_name,
            info.descriptor,
        ) {
            let values = decode_values();
            // Interpreter round i3 wave 83, lane L3
            // (`CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE`, default off): file the
            // site in the lean native table, so the next call is served at the
            // helper's first statement (`try_lean_native_door`) instead of
            // after this body's prefix. Off: one per-VM bool load.
            if vm.config.jit_reflect_door_lean_site {
                reflect_door_lean_site_fill(vm, info, info_ptr as usize, receiver_ref, callback);
            }
            // The census's `gap_in` split (`CRATONVM_REFLECT_DOOR_PHASE_STATS`).
            crate::vm::reflect_door_gate_stamp(
                vm,
                cratonvm_native_builtins::ReflectDoorGatePoint::Call,
            );
            return match crate::vm::safe_native_call(vm, thread, callback, &values) {
                Ok(Some(Value::Int(v))) => v as i64,
                Ok(Some(Value::Long(v))) => v,
                Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(Some(Value::Object(None)) | None) => 0,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // ClassLoader's resource accessors require a non-null resource name. The
    // MIC fast path dispatches on a custom loader's runtime class and can call
    // inherited JDK bytecode without consulting the ClassLoader callback that
    // owns this contract. Handle the null-only case before cache lookup; normal
    // resource calls still use the loader's virtual implementation unchanged.
    //
    // Only for a receiver that IS a ClassLoader, as at the other two sites: an
    // unrelated interface with a `getResource(String)` method (a `Res` of the
    // user's own) must run its own implementation. This site lacked the check,
    // which went unseen while a protected call site's method stayed
    // interpreted; round 11 wave 8's putstatic admission compiled one
    // (R11RtGetResourceNull: 56000 spurious NPEs).
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
        && null_resource_name_receiver_is_class_loader(
            vm,
            args_slice[0],
            info.method_name,
            info.descriptor,
        )
    {
        // §7 routing. This was a bare `NativeMethodRegistry::find`, which
        // discards the `NativeKind` and so could not tell a reviewed intrinsic
        // from a `SyntheticStub` — the exact shape §1.3 forbids invoking. It
        // now resolves kind-aware and goes through the shared admission
        // helper, which also yields the §4 census handle.
        //
        // Unlike the three cached fast paths, this site resolves per call —
        // but it is guarded by `args_slice[1] == 0`, i.e. only a `null`
        // resource name reaches it, which is the JVM-mandated NPE path and
        // never hot. The full policy round trip is affordable here precisely
        // because this edge is an error path, not a throughput path.
        //
        // Declining (`None`) falls through to the ordinary MIC resolution
        // below — byte-for-byte what already happened whenever `find` returned
        // `None`, and a path that ends in `invoke_or_native`, which is itself
        // policy-checked.
        let loader_native = vm
            .natives
            .native_methods
            .resolve_id("java/lang/ClassLoader", info.method_name, info.descriptor)
            .and_then(|id| {
                let callback = vm.natives.native_methods.callback_of(id)?;
                admit_jit_fast_native_resolved(
                    vm,
                    "java/lang/ClassLoader",
                    info.method_name,
                    info.descriptor,
                    callback,
                    Some(id),
                )
            });
        if let Some((callback, native_id)) = loader_native {
            let values = decode_values();
            count_jit_native_dispatch(vm, native_id);
            return match crate::vm::safe_native_call(vm, thread, callback, &values) {
                Ok(Some(Value::Int(v))) => v as i64,
                Ok(Some(Value::Long(v))) => v,
                Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(Some(Value::Object(None)) | None) => 0,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // Round 11 wave 13 (lane `icwide`): read through `jit_dispatch_class`, so
    // an array's id here is 0 rather than its COMPONENT's. That is what keeps
    // an array of annotation proxies off the annotation arm and an array of
    // lambda proxies off the lambda arms below (both keyed on this id, and
    // neither screened by `receiver_is_plain_object`); every other reader
    // either is screened by it or refuses class 0.
    let receiver_dispatch = jit_dispatch_class(vm, receiver_ref);
    let receiver_class_id = receiver_dispatch
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    let receiver_cid = receiver_class_id.as_u32();
    // A reference array stores its COMPONENT class id in
    // `ObjectHeader.class_id` — the same invariant the KC26 array.clone() note
    // further down calls out — so `receiver_cid` alone cannot tell a `Foo[]`
    // from a `Foo`. Per JVMS §4.4.1 an array class inherits `Object`'s method
    // table, not its component's, so every class-id-keyed fast path below must
    // refuse array receivers and let the resolving miss path (which asks
    // `virtual_dispatch_target_for_receiver`, and gets `java/lang/Object`)
    // handle them. Without this a MIC warmed on a `Foo` receiver called
    // straight into compiled `Foo.equals` with a `Foo[]` as `this`, and its
    // first `checkcast Foo` threw
    // `ClassCastException: class [LFoo; cannot be cast to class Foo` — the
    // `ResolvableType[]`/`ResolvableType` Spring Boot failure this was found
    // as. The inline machine-code cascades in `jit/src/x64.rs`,
    // `jit/src/ir_lower.rs` and `jit/src/runtime_lowering.rs` carry the same
    // guard against `ObjectHeader.kind`.
    let receiver_is_plain_object = receiver_dispatch.class_id().is_some();

    // Real-layout Matcher methods are registered natives, so they can never
    // publish a compiled entry into the ordinary MIC/PIC. Without this leaf
    // path every `find`/`start`/`end`/`group` hit decodes a heap-allocated
    // `Vec<Value>`, retries a compile probe, and performs generic virtual/native
    // resolution. Guarding the exact receiver class preserves subclass
    // overrides; the feature gate in `matcher_native_callback_uncached`
    // preserves the native opt-out, and class redefinition keeps using the
    // generic resolver.
    //
    // §7 routing — this leaf is the one HOT by-name native fast path in this
    // file that re-decides on every dispatch rather than at cache-fill time
    // (the `ClassLoader` intercept above is also unmemoized, but only a null
    // resource name reaches it, so it can afford the real thing). Wave 1
    // therefore hoisted the JDK-only question to one
    // `dispatch_policy(vm).is_jdk_only()` gate and skipped the leaf wholesale
    // under strict, because calling `admit_jit_fast_native` per dispatch would
    // put a three-string registry hash — and, in strict mode, a class-manager
    // read lock plus a hierarchy walk — on a documented 19x hot path.
    //
    // Closed 2026-08-10 by [`admit_matcher_leaf_native`], the per-triple memo
    // the wave-2 record asked for: eight `NativeCallSite` cells for the census
    // handle and a per-VM generation-keyed verdict cell for the strict
    // admission. Warm cost is two `&'static str` compares and one (Compatible)
    // or two (strict) relaxed loads, so the leaf keeps its shape while the
    // dispatch becomes counted in BOTH modes and strict regains the fast route
    // instead of merely being safe without it. See that function for the full
    // argument.
    //
    // The site's static class is asked FIRST (`matcher_leaf_site_class`): the
    // triples are plain names (`start()I`, `end()I`, `find()Z`) that unrelated
    // classes declare too, and for such a site `is_exact_matcher_class` takes
    // the class-manager read lock on every call (it memoizes only a match),
    // as does `class_was_redefined` once anything has been redefined.
    if matcher_leaf_site_class(info) && !class_was_redefined(vm, receiver_class_id) {
        if let Some((callback, native_id)) = admit_matcher_leaf_native(vm, info) {
            if is_exact_matcher_class(vm, receiver_class_id) {
                if let Some(result) = call_matcher_native_raw(
                    vm,
                    thread,
                    info,
                    receiver_ref,
                    args_slice,
                    callback,
                    native_id,
                ) {
                    // §4 census: one `Option` test and one relaxed add, on the
                    // id resolved by the memo above. Counted here rather than
                    // before the call because `call_matcher_native_raw`
                    // declines an arity it cannot build a frame for, and a
                    // declined fast path is not a dispatch.
                    count_jit_native_dispatch(vm, native_id);
                    JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.matcher_leaf_hits));
                    return result;
                }
            }
        }
    }

    // AnnotationProxy receiver: synthetic class with no bytecode methods, so the
    // cache-miss path below would resolve on it, fail the compile-probe, and
    // call `invoke_or_native` only to hit its annotation rescue — every call.
    // Route straight to the shared annotation dispatch here. Uses the lock-free
    // cached cid (warmed by `invoke_or_native`'s fast-path); before it is warmed
    // the hint is `u32::MAX` (never a real cid) so this simply falls through.
    if receiver_cid == crate::vm::annotation_proxy_cid_hint(vm) {
        mic_prof::bump(&mic_prof::MIC_LAMBDA);
        let values = decode_values();
        match crate::vm::annotation_proxy_invoke_shared(
            vm,
            thread,
            receiver_ref,
            info.method_name,
            &values[1..],
        ) {
            Ok(result) => {
                return match result {
                    Some(Value::Int(v)) => v as i64,
                    Some(Value::Long(v)) => v,
                    Some(Value::Float(f)) => f.to_bits() as i64,
                    Some(Value::Double(d)) => d.to_bits() as i64,
                    Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
                    Some(Value::Object(None)) | None => 0,
                    _ => 0,
                };
            }
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        }
    }

    // Lambda-proxy receiver: its class id is a synthetic id absent from the
    // class store, so the resolution below derives an EMPTY class name and
    // `invoke_or_native("")` surfaces as a message-less
    // `NoClassDefFoundError` — e.g. `Consumer.accept` inside a JIT-compiled
    // `CollectionUtils.forEachInReverseOrder` (JUnit5 listener notification)
    // died on the first compiled execution. Mirror the interpreter's
    // invokeinterface route: dispatch through the lambda's SAM impl_handle.
    if vm.classes.is_lambda_proxy_class(receiver_class_id) {
        mic_prof::bump(&mic_prof::MIC_LAMBDA);
        // The lambda call site's own cached invoke target. Tried FIRST: it is
        // the only arm here that does not re-derive the whole dispatch, and
        // when it declines it has done nothing.
        if let Some(result) = try_lambda_site_direct_call(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info,
            args_slice,
            vm_ptr,
            Some((mic_ptr, pic_ptr)),
        ) {
            return result;
        }
        match try_fast_lambda_int_to_double_apply(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info,
            args_slice,
        ) {
            Ok(Some(result)) => return result,
            Ok(None) => {}
            Err(error) => return handle_jit_dispatch_error(vm, thread, error, info),
        }
        let values = decode_values();
        match crate::runtime::interpreter::try_lambda_dispatch(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info.method_name,
            info.descriptor,
            &values[1..],
        ) {
            Ok(Some(result)) => {
                return match result {
                    Some(Value::Int(v)) => v as i64,
                    Some(Value::Long(v)) => v,
                    Some(Value::Float(f)) => f.to_bits() as i64,
                    Some(Value::Double(d)) => d.to_bits() as i64,
                    Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
                    Some(Value::Object(None)) | None => 0,
                    _ => 0,
                };
            }
            // SAM arity/name mismatch (e.g. a same-named default method) —
            // fall through; the CP interface fallback below dispatches the
            // default body via the static call-site class.
            Ok(None) => {}
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        }
    }

    let mic = &*(mic_ptr as *const JitMICSlot);
    // Companion to the call in `jit_invoke_dispatch`: both of this function's
    // resolution sites read `VIRTUAL_TARGET_CACHE`, so both need it revalidated
    // against the current class-identity generation first.
    flush_class_identity_dispatch_memos(vm);
    // Flush this inline cache ONCE per redefinition, not on every dispatch.
    //
    // This was `hierarchy_was_redefined(vm, receiver_class_id)`, which is
    // permanently true once any class in the receiver's superclass chain has
    // been redefined. It sat in front of an unconditional `clear_compiled_entry`
    // + `clear_entries`, so after a single `Mockito.mock(Foo.class)` every
    // subsequent call on a `Foo` erased its own MIC and PIC and then re-resolved
    // the target from scratch — for the life of the process. That is the bulk
    // of the ~30 us/call redefined-dispatch cost.
    //
    // The slot now stamps the redefinition epoch it was last validated against
    // (`JitMICSlot::redefine_epoch`, in what used to be padding). A mismatch
    // means a redefinition has happened since this slot was populated, so the
    // cached entry may name evicted code: flush, restamp, and resolve normally.
    // A match means the slot was populated after the most recent redefinition
    // and is as trustworthy as any other inline cache. Steady state is one
    // relaxed load and a compare.
    let epoch_now = vm.jit.jit_cache.redefine_epoch();
    // Read first; write only when the epoch actually moved.
    //
    // This was an unconditional `swap`, i.e. a locked read-modify-write on the
    // MIC slot's first eight bytes on EVERY helper-side dispatch — including
    // every run in which nothing is ever redefined, where it stored the value
    // that was already there. `redefine_epoch` lives at offset 4 and
    // `cached_class_id` at offset 0 (see `JitMICSlot`'s layout note), so the
    // store dirtied the very line the inline machine-code MIC cascade loads on
    // every probe, and did so from each thread sharing the site.
    //
    // The semantics are unchanged: a load that already equals `epoch_now`
    // proves the slot was populated after the most recent redefinition, which
    // is exactly what the `swap`'s `prev == epoch_now` arm concluded.
    let redefine_jit_quiesced = if mic
        .redefine_epoch
        .load(std::sync::atomic::Ordering::Acquire)
        == epoch_now
    {
        false
    } else {
        mic.redefine_epoch
            .swap(epoch_now, std::sync::atomic::Ordering::AcqRel)
            != epoch_now
    };
    if redefine_jit_quiesced {
        mic.clear_compiled_entry();
        if pic_ptr != 0 {
            let pic = &*(pic_ptr as *const JitPICSlot);
            pic.clear_entries();
        }
        VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
    }
    // Interpreter-only mode (a JVMTI interpreter-only event, or a JDWP
    // breakpoint / step / suspension): every arm below that enters a compiled
    // callee, or compiles one to publish into the MIC/PIC, stands down with the
    // redefinition quiescence, exactly as `jit_invoke_dispatch_body`'s arms do.
    // The callee then runs through `invoke_or_native`, whose interpreter doors
    // decline compiled code the same way, so MethodEntry/MethodExit and
    // breakpoints are delivered. The inline machine-code cascade never reaches
    // this helper while its slot holds an entry; the edge into the mode
    // flushes every slot (`JitCache::clear_inline_caches`, wave 9), and while
    // the mode holds this helper publishes nothing. Interpreter round i1
    // wave 6, L7.
    let compiled_entry_forbidden =
        redefine_jit_quiesced || crate::runtime::interpreter::jvmti_requires_interpreter(vm);
    // This helper publishes raw compiled entries into `VIRTUAL_DISPATCH_CACHE`
    // for callees the MIC/PIC cannot hold, so it owes that cache the same
    // generation revalidation `jit_invoke_dispatch` does.
    flush_raw_entry_dispatch_caches();
    let cached_cid = mic
        .cached_class_id
        .load(std::sync::atomic::Ordering::Acquire);

    // A callee that declares an exception table is barred from the MIC and the
    // PIC (`mic_callee_has_exception_table`), so nothing ever writes this slot's
    // class id for it and **every** call to it is a "miss" — for the life of the
    // process. `VIRTUAL_DISPATCH_CACHE` exists precisely to serve those callees,
    // and the consult used to sit on the miss path *behind* `mic.record_miss`
    // and `virtual_dispatch_target_cached` — a memoized-but-real class-name
    // resolution and round-trip test the warm hit does not need. It is hoisted
    // here, ahead of the megamorphic PIC probe as well, because a hit means a
    // compiled Java callee was published for exactly this `(site, receiver
    // class)` pair and none of the work below can improve on that.
    //
    // Ordering obligations, all already discharged above: `forward_jit_reference_args`
    // has canonicalised the arguments, `flush_raw_entry_dispatch_caches` and
    // `flush_class_identity_dispatch_memos` have revalidated the cache against
    // the current generations, and `redefine_jit_quiesced` carries the
    // redefinition epoch. `cached_cid != receiver_cid` keeps the MIC's own
    // machine-callable entry first when it has one.
    //
    // The consult it replaces was additionally gated on `cacheable_receiver &&
    // globally_named`, and dropping those here is sound rather than convenient:
    // both are PUBLICATION conditions (`publish_mic_rust_cached_entry` is called
    // under them), and the key carries the receiver's own class id — so a hit
    // means a compiled callee was published for exactly this `(site, receiver
    // class)` pair while those conditions held. Re-testing them would mean
    // calling `virtual_dispatch_target_cached`, which is the work this hoist
    // exists to skip. What is kept is the receiver guard the MIC's own fast path
    // uses one branch below — `receiver_is_plain_object` — so this arm is no
    // weaker than the machine-code entry it stands in for.
    //
    // Worth 11x on a monomorphic interface call whose callee holds a never-taken
    // `try`/`catch`: 220 ns against 19 ns for the identical call without one
    // (`probes/NativeFunnelFloorProbe.java`, ABBA on one binary). Most of that
    // 11x is the inline machine-code cascade this callee cannot be published
    // into at all, which is a separate and much larger piece of work; this is
    // the part that can be taken off the Rust route without touching codegen.
    if cached_cid != receiver_cid && receiver_is_plain_object && !compiled_entry_forbidden {
        if let Some(rc) = try_mic_rust_cached_entry(
            vm,
            thread,
            info,
            info_ptr,
            receiver_cid,
            vm_ptr,
            args_slice,
            args_forwarded,
        ) {
            disp_census::note(disp_census::MIC_RUST_CACHE);
            return rc;
        }
    }

    if crate::runtime::env_cache::jit_mic_dbg() {
        let (pic_classes, pic_entries, pic_contexts) = if pic_ptr == 0 {
            (
                [0; cratonvm_jit::JIT_PIC_ENTRIES],
                [0; cratonvm_jit::JIT_PIC_ENTRIES],
                [false; cratonvm_jit::JIT_PIC_ENTRIES],
            )
        } else {
            let pic = &*(pic_ptr as *const JitPICSlot);
            let ways: [(u32, u64, bool); cratonvm_jit::JIT_PIC_ENTRIES] =
                std::array::from_fn(|i| pic.way(i).unwrap_or((0, 0, false)));
            (
                std::array::from_fn(|i| ways[i].0),
                std::array::from_fn(|i| ways[i].1),
                std::array::from_fn(|i| ways[i].2),
            )
        };
        eprintln!(
            "[JIT_MIC] {}.{}{} cached_cid={} recv_cid={} entry={} pic_ptr={:#x} pic_classes={:?} pic_entries={:?} pic_contexts={:?}",
            info.class_name,
            info.method_name,
            info.descriptor,
            cached_cid,
            receiver_cid,
            mic.cached_entry().0,
            pic_ptr,
            pic_classes,
            pic_entries,
            pic_contexts,
        );
    }

    // Megamorphic helper fast path. Generated code probes only the four-entry
    // inline PIC; receiver types beyond that capacity land here. The PIC keeps
    // a bounded secondary target cache so those misses still avoid repeated
    // class hierarchy resolution and compile-cache probing.
    if cached_cid != receiver_cid
        && receiver_is_plain_object
        && pic_ptr != 0
        && direct_virtual_compiled_callee_entry_enabled()
        && !compiled_entry_forbidden
    {
        let pic = &*(pic_ptr as *const JitPICSlot);
        if let Some((entry, needs_context)) = pic.lookup_megamorphic(receiver_cid) {
            // Round 11 wave 18 (lane hashmap): a hashed way keeps the body that
            // was current when it was installed, and nothing re-points it when
            // a later publication of the same method SUPERSEDES that body
            // (`JitCache::put` marks it `superseded` and touches no inline
            // cache). So a callee first compiled badly -- a deferred `new`
            // resolved per execution, a C1 body before its C2 -- stayed the
            // body this site called for the life of the caller. The owner this
            // arm pins for the call anyway (it is the pin
            // `try_call_compiled_entry_reentrant` takes) answers the question
            // for free: a superseded or retired body is not called, the call
            // takes the resolving miss arm below, which finds the current body,
            // retires the stale way and installs the new one (or, with no free
            // way, hands it to the per-thread entry cache the hoisted consult
            // above serves).
            let pinned = if entry != 0 {
                cratonvm_jit::pin_jit_code_range_owner(entry as usize)
            } else {
                None
            };
            let stale = pinned.as_deref().is_some_and(|cm| {
                cm.superseded.load(std::sync::atomic::Ordering::Acquire)
                    || cm.retired.load(std::sync::atomic::Ordering::Acquire)
            });
            if entry != 0 && !stale {
                disp_census::note(disp_census::MIC_PIC);
                mic_prof::bump(&mic_prof::MIC_HIT_ENTRY);
                // A table callee's sentinel can RE-RUN it at its handler with
                // these arguments, so root them across the call and its
                // servicing when this site ever published one (the site flag
                // covers the PIC too). See `try_mic_rust_cached_entry`.
                // gcg g6/jm: not for a body that owns its arguments.
                let owns = pinned
                    .as_deref()
                    .is_some_and(cratonvm_jit::callee_body_owns_its_arguments);
                let pins = (args_forwarded
                    || door_pins_call_args(
                        mic.callee_rerun_pins(),
                        owns,
                        door_owned_args_unpinned_enabled(vm),
                    ))
                .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
                // `try_call_compiled_entry_reentrant`, with the pin taken
                // above instead of a second registry round trip.
                note_cached_entry_arm(&CACHED_ENTRY_REGISTRY_PINS);
                let called = call_compiled_entry_under_owner(
                    pinned.as_deref(),
                    entry as usize,
                    needs_context,
                    vm_ptr,
                    args_slice,
                );
                if let Some(result) = called {
                    // A megamorphically-dispatched callee deopts exactly like a
                    // monomorphically-dispatched one, and this arm used to
                    // return its `i64::MIN` straight to the compiled caller —
                    // which reads it as its OWN deopt, so the callee's stashed
                    // frame surfaces at an unrelated sink that de-speculates
                    // the wrong method and cannot resume it. See
                    // `handle_compiled_callee_deopt_sentinel`.
                    let result = if result == i64::MIN {
                        handle_rust_armed_callee_sentinel(
                            vm,
                            thread,
                            info,
                            ClassId::new(receiver_cid),
                            args_slice,
                            pins.as_ref(),
                        )
                        .unwrap_or(result)
                    } else {
                        result
                    };
                    if let Some(pins) = pins {
                        pins.release(thread);
                    }
                    return result;
                }
                if let Some(pins) = pins {
                    pins.release(thread);
                }
            }
        }
        // Round 12 wave 2 (lane calls, W2-1): the site's own caches missed;
        // ask the VM's shared megamorphic table, which every other site of
        // this method fills, before the full resolution below.
        note_unbound_selector_table_held(vm, info, pic, receiver_cid);
        if let Some(rc) = try_mega_dispatch_table_entry(
            vm,
            thread,
            info,
            pic,
            receiver_class_id,
            vm_ptr,
            args_slice,
            epoch_now,
        ) {
            disp_census::note(disp_census::MIC_MEGA_TABLE);
            return rc;
        }
    }

    // --- Monomorphic Inline Cache: fast path ---
    // If the receiver ClassId matches the cached value AND we have a cached
    // entry pointer, dispatch directly without any class_manager lookup or
    // method resolution.  This is the zero-overhead dispatch path.
    // `is_empty_class_id` (round 9 wave 2): a fresh slot holds
    // `JitMICSlot::EMPTY_CLASS_ID`, a zeroed one `0`; neither is a cached class.
    if cached_cid == receiver_cid
        && !JitMICSlot::is_empty_class_id(cached_cid)
        && receiver_is_plain_object
    {
        mic.note_dispatch_hit();

        // Try the cached compiled entry pointer (true inline cache hit). The
        // entry and its ABI flag are decoded from ONE load of the tagged word,
        // exactly as generated code does.
        let (entry, needs_ctx) = mic.cached_entry();
        if direct_virtual_compiled_callee_entry_enabled() && entry != 0 && !compiled_entry_forbidden
        {
            // Direct call to the compiled callee — same ABI as `jit_invoke_dispatch`
            // uses after a JIT-cache hit (receiver + params in `args_slice`, optional
            // leading `vm_ptr` when the entry word's context tag is set).  **Do not** pass
            // `(vm_ptr, info_ptr, args_ptr, num_args)` here; that was a mis-invocation
            // that corrupts the stack and surfaces as Windows AV / Linux SIGSEGV.
            // Register-table call straight off the raw arg slots — no `Value`
            // decode on this hot path. `try_call_compiled_entry` returns
            // `None` exactly when the callee has more args than the transmute
            // tables cover (16 ABI words, 8 under its kill switch); only then decode the args
            // and route through the interpreter instead of silently returning
            // 0 from a truncated register-arg table.
            mic_prof::bump(&mic_prof::MIC_HIT_ENTRY);
            // As the megamorphic arm above: root the arguments across a call
            // whose sentinel may re-run a table callee with them. gcg g6/jm:
            // not for a body that owns its arguments (asked only when the
            // site flag would pin: one registry lookup on that path).
            let pins = (args_forwarded
                || (mic.callee_rerun_pins()
                    && door_pins_call_args(
                        true,
                        cratonvm_jit::direct_callee_owns_its_arguments(entry as usize),
                        door_owned_args_unpinned_enabled(vm),
                    )))
            .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
            let rc_opt = {
                let _g = mic_prof::CycGuard::new(&mic_prof::CYC_HIT_ENTRY_CALL);
                try_call_compiled_entry_reentrant(entry as usize, needs_ctx, vm_ptr, args_slice)
            };
            if let Some(rc) = rc_opt {
                let rc = if rc == i64::MIN {
                    handle_rust_armed_callee_sentinel(
                        vm,
                        thread,
                        info,
                        receiver_class_id,
                        args_slice,
                        pins.as_ref(),
                    )
                    .unwrap_or(rc)
                } else {
                    rc
                };
                if let Some(pins) = pins {
                    pins.release(thread);
                }
                return rc;
            }
            if let Some(pins) = pins {
                pins.release(thread);
            }
            let values = decode_values();
            return bail_to_interpreter(vm, thread, info, &values);
        }

        mic_prof::bump(&mic_prof::MIC_HIT_NOENTRY);
        // gci i12/mf: a (site, receiver class) whose slow call already ended
        // in a table-bound JNI native goes straight to the door, skipping the
        // resolution, the compile probe and the by-name tail below
        // (`CRATONVM_JNI_DISPATCH_MEMO`). One byte compare when it is off.
        if let Some(hit) = jit_jni_vsite_lookup(
            vm,
            thread,
            info,
            info_ptr as usize,
            receiver_dispatch.class_id(),
        ) {
            let mut values = decode_values();
            return jit_jni_vsite_call(
                vm,
                thread,
                info,
                &hit,
                &mut values[..],
                &hit.dispatch_class,
            );
        }
        // Entry not cached yet — use cached class name for fast dispatch.
        //
        // KC26 array.clone() bug: array receivers store the COMPONENT class
        // id in their header (per the documented invariant in
        // `runtime/interpreter.rs`). Falling through to
        // `get_class(receiver_class_id).name` would resolve dispatch on the
        // component (e.g. `OptionCategory`/`Enum`) and surface
        // `Enum.clone() → CloneNotSupportedException` for every array clone
        // of an enum type. Per JVMS §4.4.1, array classes inherit their
        // method table from `Object`; short-circuit accordingly.
        let dispatch_target = virtual_dispatch_target_cached(
            vm,
            receiver_ref,
            info,
            jit_site_key(vm.vm_identity, info_ptr as usize),
        );
        if dispatch_target.selection_error {
            if let Some(error) = jit_selection_failure(vm, thread, receiver_ref, info) {
                return handle_jit_dispatch_error(vm, thread, error, info);
            }
        }
        // Round 17 wave 2 (lane mega2, MEGA17-3, opt-in
        // `CRATONVM_JIT_MEGA_CACHE_RETARGETED`): a receiver whose §5.4.6
        // selection moved to a superclass publishes like a receiver-named one,
        // with its body compiled for the SELECTED class (`compile_cid`). Every
        // cache below is keyed by the receiver's class id at this site (or its
        // selector), and selection is a function of exactly those, so the entry
        // can never answer for a receiver whose selection differs. See
        // `CachedDispatchTarget::selected_cid`.
        // gci i12/mf: a selection error never fills a compiled JNI site row.
        let jni_vsite_fill_ok = !dispatch_target.selection_error;
        let cacheable_receiver = dispatch_target.publishable();
        let compile_cid = dispatch_target.compile_cid(receiver_cid);
        let globally_named = dispatch_target.globally_named;
        // Round 15 wave 2 (lane mega, MG7-1): the resolution passed; bind the
        // site's shared-table selector now (switch off: nothing happens).
        if cacheable_receiver && pic_ptr != 0 && !compiled_entry_forbidden {
            bind_mega_selector_at_resolution(
                vm,
                info,
                &*(pic_ptr as *const JitPICSlot),
                receiver_cid,
            );
        }
        // An entryless slot can be retargeted by another thread after the
        // class-id probe above. Its cached class name is therefore not a
        // coherent companion to this receiver; resolve from the receiver
        // itself until there is a callable entry to use.
        let class_name = dispatch_target.class_name;

        // See `try_mic_rust_cached_entry`. Its key carries the receiver's own
        // class id and every entry in it was compiled for that id, so
        // `globally_named` is not a condition here (as at the hoisted consult).
        if cacheable_receiver && !compiled_entry_forbidden {
            if let Some(rc) = try_mic_rust_cached_entry(
                vm,
                thread,
                info,
                info_ptr,
                receiver_cid,
                vm_ptr,
                args_slice,
                args_forwarded,
            ) {
                return rc;
            }
        }
        if cv_trace {
            eprintln!(
                "[cv-mic-hit-noentry] resolved_class={} recv_cid={} cacheable={}",
                class_name, receiver_cid, cacheable_receiver
            );
        }

        // `decode_values` yields exactly `[receiver, args...]` — the full
        // argument vector `invoke_or_native` expects. (This path previously
        // rebuilt it twice via `values[1..].to_vec()` + a fresh prepend.)
        // Mutable: the compile probe below re-reads its objects from pins.
        let mut full_args = decode_values();

        // Try to compile callee for next time (populate the MIC's tagged entry word).
        //
        // VIRTUAL DISPATCH FIX: resolve the callee from the RECEIVER's class
        // (`class_name`, derived from `receiver_class_id` above), NOT from
        // `info.class_name` (the *static* call-site type). When the receiver
        // overrides a concrete superclass method — e.g. a `Long` reached
        // through an `Object`-typed `equals(Object)Z` call site — the static
        // type resolves to `Object.equals` (identity `==`) instead of
        // `Long.equals` (value comparison). The MIC then cached that identity
        // entry against the receiver's class id and invoked it on every
        // monomorphic hit, so two distinct but equal-valued boxes compared
        // unequal (bc-java InterleaveTest, junit assertEquals(Object,Object)).
        // `find_method_recursive` (inside `try_jit_compile_callee`) walks up
        // from the receiver class to the real override.
        // LOADER IDENTITY: the by-name door (`try_jit_compile_callee`) resolves
        // the callee BY NAME. When the receiver's class is not the class that name globally
        // resolves to, that hands back ANOTHER loader's copy of the method, and
        // the entry is then cached against THIS receiver's class id — so every
        // monomorphic hit machine-CALLs a body compiled for a different copy.
        //
        // Measured 2026-08-11 on ApplicationContextAotGeneratorTests: each
        // `@CompileWithForkedClassLoader` test defines its own
        // `DynamicJavaFileManager`, so one run showed that name resolving to
        // eight-plus distinct class ids (2690, 8510, 10377, 14030, 15877,
        // 17724, 19569, 21414, ...). The copy that got compiled has its
        // `instanceof DynamicClassFileObject` site interned against ITS OWN
        // `DynamicClassFileObject` id, so the check correctly answered false for
        // the receiver's file object, `super.inferBinaryName` ran, and
        // `JavacFileManager` threw on a file object it did not create.
        //
        // That was answered first by refusing any receiver whose name is not
        // globally unique (`globally_named`). Since interpreter round i1 wave 7
        // the compile is asked for the receiver's OWN class id
        // (`try_jit_compile_callee_for_class`), which resolves the method from
        // that class and probes/publishes under its id, so every such receiver
        // gets its own copy's body and the gate is gone.
        // Has the compiler already declined this callee for this receiver? See
        // `MIC_COMPILE_DECLINED` — the probe below is the single most expensive
        // thing on this arm and it answered `None` on every one of 6.59 M calls
        // in the measured run.
        let declined_key = (
            jit_site_key(vm.vm_identity, info_ptr as usize),
            receiver_cid,
        );
        let skip_probe =
            MIC_COMPILE_DECLINED.with(|m| match m.borrow_mut().get_mut(&declined_key) {
                Some(n) => {
                    *n = n.wrapping_add(1);
                    *n % MIC_COMPILE_PROBE_RETRY != 0
                }
                None => false,
            });
        if skip_probe {
            note_cached_entry_arm(&MIC_COMPILE_PROBE_MEMO_SKIPS);
        }
        let compile_res = if skip_probe
            || !direct_virtual_compiled_callee_entry_enabled()
            || compiled_entry_forbidden
            || !cacheable_receiver
        {
            // Attribute the skip. All of these land in `pub_probe_none`
            // below without a probe ever running, which is why that counter
            // reading equal to `hit_noentry` could not distinguish "the compiler
            // refused every callee" from "we never asked".
            if skip_probe {
                // Already attributed above, and deliberately NOT folded into any
                // `not_probed_*` counter: those name gates, this names a memo.
            } else if !direct_virtual_compiled_callee_entry_enabled() {
                mic_prof::bump(&mic_prof::NOT_PROBED_DISABLED);
            } else if compiled_entry_forbidden {
                // A redefinition, or interpreter-only mode.
                mic_prof::bump(&mic_prof::NOT_PROBED_REDEFINE);
            } else {
                mic_prof::bump(&mic_prof::NOT_PROBED_UNCACHEABLE);
            }
            None
        } else {
            let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
            // The compile can run `<clinit>` and collect: root the decoded
            // objects across it and re-read them, so the interpreter route
            // below does not run on pre-move addresses. See [`CompileArgPins`].
            let pins = CompileArgPins::pin_values(vm, thread, &full_args);
            let probed = crate::runtime::interpreter::try_jit_compile_callee_for_class(
                vm,
                // The receiver's own class, or the §5.4.6-selected one (MEGA17-3).
                ClassId::new(compile_cid),
                &class_name,
                info.method_name,
                info.descriptor,
                // JIT-dispatch callee compile — optimized (C2-equivalent) tier.
                true,
            );
            pins.release_values(thread, &mut full_args);
            if probed.is_none() {
                // A probe ACTUALLY ran and the compiler declined. This is the
                // only one of the four `pub_probe_none` causes that names a
                // compile refusal, and `CRATONVM_DBG=callee-probe` says which.
                mic_prof::bump(&mic_prof::PROBE_RETURNED_NONE);
                MIC_COMPILE_DECLINED.with(|m| {
                    m.borrow_mut().entry(declined_key).or_insert(0);
                });
            } else {
                // It compiles now — drop the refusal so a later eviction of the
                // published entry re-probes immediately instead of waiting out
                // the retry interval.
                MIC_COMPILE_DECLINED.with(|m| {
                    m.borrow_mut().remove(&declined_key);
                });
            }
            probed
        };
        // `full_args[0]` is the receiver, re-read from its pin if the probe ran.
        let receiver_ref = match full_args.first() {
            Some(Value::Object(Some(receiver))) => *receiver,
            _ => receiver_ref,
        };
        if cv_trace {
            eprintln!(
                "[cv-mic-compile] site={}.{}{} recv_cid={} resolved_class={} cacheable={} globally_named={} compiled={}",
                info.class_name,
                info.method_name,
                info.descriptor,
                receiver_cid,
                class_name,
                cacheable_receiver,
                globally_named,
                compile_res.is_some(),
            );
        }
        // Keep handler-bearing methods on the helper path. A raw compiled
        // entry can leave a pending exceptional frame that the caller cannot
        // safely resume while the HTTP request is still active.
        if compile_res.is_none() {
            mic_prof::bump(&mic_prof::PUB_PROBE_NONE);
        }
        // Read before the destructuring `if let` below partially moves it.
        let callee_has_compiled_code = compile_res.is_some();
        if let Some((_callee_pin, entry_ptr, needs_ctx)) = compile_res {
            // `_callee_pin` holds the callee artifact across the publications
            // below: `update`/`install` take their own keep-alive by resolving
            // the entry, and that resolution can only succeed while the
            // artifact is alive.
            // jit-invokedynamic-groovy-regression fix: also never publish an
            // artifact containing an unconditional invokedynamic trap — the
            // inline MIC/PIC cascade would machine-CALL it, letting the trap's
            // sentinel + stashed frame bail through the compiled caller's
            // epilogue past the only point able to resume it precisely.
            let callee_barred_by_table = !mic_publish_exception_table_callees()
                && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info);
            // Read off the artifact being published, not re-probed by name: the
            // name-keyed probe reads another loader's row when the receiver's
            // class name is not globally unique.
            let callee_has_indy_trap = _callee_pin.has_indy_trap;
            if callee_barred_by_table && !callee_has_indy_trap && cacheable_receiver {
                publish_mic_rust_cached_entry(
                    vm.vm_identity,
                    info_ptr,
                    receiver_cid,
                    entry_ptr,
                    needs_ctx,
                    true,
                );
            }
            if callee_barred_by_table || callee_has_indy_trap {
                mic_prof::bump(&mic_prof::PUB_BARRED);
            }
            if !callee_barred_by_table && !callee_has_indy_trap {
                mic_prof::bump(&mic_prof::PUB_PUBLISHED);
                mark_site_rerun_pins_before_publish(vm, mic, receiver_cid, info, entry_ptr != 0);
                // Publish through `update`, never with a raw store: `update` is
                // the only writer that also resolves and RETAINS the callee's
                // `Arc<CompiledMethod>` in the slot's `compiled_owner`.
                //
                // Until 2026-07-27 this branch stored the entry word
                // directly, so a slot reached through the "class cached, target
                // unresolved" shape (what `prepopulate` seeds and what
                // `clear_compiled_entry` leaves behind after every
                // invalidation) ended up holding a RAW entry pointer with no
                // keep-alive. The next tier-up `put` for that callee replaced
                // its shard snapshot, dropped the last `Arc`, and `munmap`ped
                // the body — while the inline `MOV R11,[mic+8]; CALL R11`
                // cascade emitted by `jit/src/x64.rs` still called it. That is
                // the ElasticSearch `NodeConnectionsServiceTests` SIGSEGV
                // (`rip == r11 ==` first byte of a retired code mapping) and,
                // once the address was recycled by a later allocation, the
                // json-smart "re-parse returned another method's result"
                // corruption.
                // t1 wave 4 (lane Y2): a refusal here reaches this VM's report.
                let _jdk_only_scope = jdk_only_ic_publication_scope(
                    vm,
                    crate::vm::dispatch_policy(vm).is_jdk_only(),
                );
                // MEGA5-18-4: a gate-first megamorphic site never reads its MIC.
                if !mic_publication_skipped(pic_ptr) {
                    mic.update(
                        receiver_cid,
                        &class_name,
                        entry_ptr as u64,
                        needs_ctx,
                        crate::vm::dispatch_policy(vm).is_jdk_only(),
                    );
                }
                // CRIT-1 — also populate the co-allocated PIC so the
                // inline 4-way cascade in `jit/src/x64.rs` hits on the
                // next invocation. Without this the cascade's empty
                // (class_id == 0) slots always fail and every dispatch
                // pays the full helper cost. We only install when we
                // actually have an entry_ptr to publish; a 0 entry_ptr
                // in a PIC slot would force the inline cascade to call
                // through a null function pointer.
                if pic_ptr != 0 && entry_ptr != 0 {
                    let pic = &*(pic_ptr as *const JitPICSlot);
                    // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`):
                    // a MIC-first site's MIC already serves this class inline;
                    // its four ways are kept for four other classes.
                    if !pic_way_redundant_with_mic(mic, pic, receiver_cid)
                        && pic.install(
                            receiver_cid,
                            &class_name,
                            entry_ptr as u64,
                            needs_ctx,
                            crate::vm::dispatch_policy(vm).is_jdk_only(),
                        )
                    {
                        disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
                        request_ic_grace_if_due(vm, Some(thread.thread_id));
                    }
                    // Round 12 wave 2 (lane calls, W2-1): and the VM's shared
                    // megamorphic table, for every other site of this method.
                    publish_mega_dispatch_entry(
                        vm,
                        info,
                        pic,
                        receiver_cid,
                        entry_ptr as u64,
                        needs_ctx,
                        epoch_now,
                    );
                }
                withdraw_ic_publication_if_redefined(
                    vm,
                    epoch_now,
                    mic,
                    (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot)),
                );
            }
            // Round 12 wave 1 (lane `calls`, W18-3): as the miss arm below,
            // enter the body the probe just answered.
            if entry_ptr != 0
                && cacheable_receiver
                && !callee_has_indy_trap
                && !compiled_entry_forbidden
            {
                if let Some(rc) = call_body_the_mic_probe_just_found(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    entry_ptr,
                    needs_ctx,
                    vm_ptr,
                    &full_args[..],
                    // gch h1/jp: the caller's block, unless the door read a
                    // forwarded Rust copy of it.
                    forwarded_args.is_none().then_some(args_slice),
                    callee_barred_by_table || mic.callee_rerun_pins(),
                    epoch_now,
                ) {
                    disp_census::note(disp_census::MIC_MISS_CALLED_COMPILED);
                    return rc;
                }
            }
        }

        // The uncompiled-callee fast path. Reached only after the MIC entry,
        // the PIC, `VIRTUAL_DISPATCH_CACHE` and the compile probe have all
        // declined, and gated on `compile_res.is_none()` so a callee that DOES
        // have compiled code is never diverted into the interpreter — the
        // by-name `invoke_or_native` below would have entered that code.
        // nio21 w19 L2: the selected method compiled under its DECLARING
        // class's key (an inherited `final` method such as
        // `ReentrantLock$Sync.tryRelease` reached on a `NonfairSync`), which the
        // receiver-keyed probe above cannot see. See
        // `mic_tail_call_selected_declaring_body`.
        if !callee_has_compiled_code && !compiled_entry_forbidden && receiver_is_plain_object {
            if let Some(rc) = mic_tail_call_selected_declaring_body(
                vm,
                thread,
                info,
                info_ptr,
                receiver_class_id,
                vm_ptr,
                &full_args[..],
                forwarded_args.is_none().then_some(args_slice),
                mic.callee_rerun_pins(),
                epoch_now,
                Some((mic_ptr, pic_ptr, &*class_name)),
            ) {
                disp_census::note(disp_census::MIC_MISS_CALLED_COMPILED);
                return rc;
            }
        }
        if !callee_has_compiled_code {
            if let Some(r) = try_jit_virtual_bytecode_callee(
                vm,
                thread,
                info,
                jit_site_key(vm.vm_identity, info_ptr as usize),
                receiver_ref,
                receiver_class_id,
                &full_args,
                false,
            ) {
                return match r {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
        }
        // gci i12/mf: the compiled virtual call-site rows' fill probe brackets
        // the by-name tail (armed only with the memo on).
        let jni_vsite_probe = crate::vm::vm_exec::jni_jit_vsite_probe_arm(
            vm,
            thread,
            info.method_name,
            info.descriptor,
        );
        let invoke_res = {
            let _g = mic_prof::CycGuard::new(&mic_prof::CYC_INVOKE);
            crate::vm::dbg_dispatch_tally(
                "jit_mic_tail",
                &class_name,
                info.method_name,
                info.descriptor,
            );
            // gch h1/jp (fr review item 3): the S111r12 rescue below resends
            // `full_args`; a failed attempt re-reads it from pins.
            crate::vm::invoke_or_native_keeping_args_current(
                vm,
                thread,
                &class_name,
                info.method_name,
                info.descriptor,
                &mut full_args[..],
                !info.class_name.is_empty() && &*class_name != info.class_name,
            )
        };
        if let Some(save) = jni_vsite_probe {
            crate::vm::vm_exec::jni_jit_vsite_probe_finish(
                vm,
                thread,
                save,
                ClassId::new(info.declaring_class_id),
                crate::vm::vm_exec::jni_jit_vsite(info_ptr as usize),
                receiver_class_id,
                &class_name,
                info.class_name,
                info.method_name,
                info.descriptor,
                invoke_res.is_ok() && jni_vsite_fill_ok && receiver_is_plain_object,
            );
        }
        // S111r12 — JIT MIC fast-path rescue: same CP-class fallback
        // as the cache-miss branch below (see comment there).
        //
        // Round-fix (Jetty): a thrown exception from the MIC dispatch must
        // be routed through `handle_jit_dispatch_error` (stash + return the
        // `i64::MIN` deopt sentinel) — the old code merely logged it and
        // returned 0, silently swallowing the exception and letting the JIT
        // caller run on with a bogus value.
        let result = match invoke_res {
            Ok(v) => v,
            Err(e)
                if !info.class_name.is_empty()
                    && &*class_name != info.class_name
                    && is_dispatch_no_such_method_miss(
                        &e,
                        &class_name,
                        info.method_name,
                        info.descriptor,
                    ) =>
            {
                match crate::vm::invoke_or_native(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &full_args,
                ) {
                    Ok(v) => v,
                    Err(e2) => {
                        return handle_jit_dispatch_error(vm, thread, e2, info);
                    }
                }
            }
            Err(e) => {
                return handle_jit_dispatch_error(vm, thread, e, info);
            }
        };

        if cv_trace {
            eprintln!(
                "[cv-mic-hit-result] is_null={}",
                matches!(result, Some(Value::Object(None)) | None)
            );
        }
        return match result {
            Some(Value::Int(v)) => v as i64,
            Some(Value::Long(v)) => v,
            Some(Value::Float(f)) => f.to_bits() as i64,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
            Some(Value::Object(None)) | None => 0,
            _ => 0,
        };
    }

    // --- Cache miss: full resolution + update cache ---
    mic.note_dispatch_miss();

    // gci i12/mf: as the entryless-hit arm above, for a receiver class the
    // monomorphic slot does not hold.
    if let Some(hit) = jit_jni_vsite_lookup(
        vm,
        thread,
        info,
        info_ptr as usize,
        receiver_dispatch.class_id(),
    ) {
        let mut values = decode_values();
        return jit_jni_vsite_call(vm, thread, info, &hit, &mut values[..], &hit.dispatch_class);
    }

    let dispatch_target = virtual_dispatch_target_cached(
        vm,
        receiver_ref,
        info,
        jit_site_key(vm.vm_identity, info_ptr as usize),
    );
    if dispatch_target.selection_error {
        if let Some(error) = jit_selection_failure(vm, thread, receiver_ref, info) {
            return handle_jit_dispatch_error(vm, thread, error, info);
        }
    }
    // Round 17 wave 2 (lane mega2, MEGA17-3): as in the entryless-hit arm above.
    // gci i12/mf: as in the entryless-hit arm above.
    let jni_vsite_fill_ok = !dispatch_target.selection_error;
    let cacheable_receiver = dispatch_target.publishable();
    let compile_cid = dispatch_target.compile_cid(receiver_cid);
    let class_name = dispatch_target.class_name;
    // Round 15 wave 2 (lane mega, MG7-1): as in the entryless-hit arm above.
    if cacheable_receiver && receiver_is_plain_object && pic_ptr != 0 && !compiled_entry_forbidden {
        bind_mega_selector_at_resolution(vm, info, &*(pic_ptr as *const JitPICSlot), receiver_cid);
    }
    if cv_trace {
        eprintln!(
            "[cv-mic-miss] resolved_class={} recv_cid={} cacheable={}",
            class_name, receiver_cid, cacheable_receiver
        );
    }

    // The `VIRTUAL_DISPATCH_CACHE` consult that used to be here has moved ABOVE
    // `record_miss` and `virtual_dispatch_target_cached` — see the note at the
    // `cached_cid` load. It is not merely reordered: the entries this cache can
    // hold are published only for a `cacheable_receiver` plain-object
    // receiver, which is a subset of the condition the hoisted consult tests,
    // so nothing that could hit here fails to hit there.
    mic_prof::bump(&mic_prof::MIC_MISS);
    // Try to compile callee for cached entry. Resolve by the RECEIVER's class
    // (`class_name`), not the static `info.class_name` — see the matching
    // VIRTUAL DISPATCH FIX in the cache-hit branch above. `class_name` here is
    // an `Arc<str>`; deref to `&str` for the resolver.
    // LOADER IDENTITY: the by-name door (`try_jit_compile_callee`) resolves the
    // callee BY NAME. When the receiver's class is not the class that name
    // globally resolves to, that hands back ANOTHER loader's copy of the
    // method, and the entry is then cached against THIS receiver's class id —
    // so every monomorphic hit machine-CALLs a body compiled for a different
    // copy.
    //
    // Measured 2026-08-11 on ApplicationContextAotGeneratorTests: each
    // `@CompileWithForkedClassLoader` test defines its own
    // `DynamicJavaFileManager`, so one run showed that name resolving to
    // eight-plus distinct class ids (2690, 8510, 10377, 14030, 15877,
    // 17724, 19569, 21414, ...). The copy that got compiled has its
    // `instanceof DynamicClassFileObject` site interned against ITS OWN
    // `DynamicClassFileObject` id, so the check correctly answered false for
    // the receiver's file object, `super.inferBinaryName` ran, and
    // `JavacFileManager` threw on a file object it did not create.
    //
    // The compile is asked for the receiver's OWN class id, so it resolves the
    // method from that class and probes/publishes under its id; see the
    // matching note in the cache-hit branch (the former `globally_named` gate).
    //
    // Decoded BEFORE the probe, not after it: the probe can run `<clinit>` and
    // collect, and a decode afterwards would validate and pass on the pre-move
    // words of the staging block, which is in no oop map. The decoded objects
    // are pinned across the probe instead. See [`CompileArgPins`].
    // `decode_values` yields exactly the `[receiver, args...]` vector
    // `invoke_or_native` expects.
    let mut full_args = decode_values();
    // Round 11 wave 18 (lane hashmap): the same `MIC_COMPILE_DECLINED` throttle
    // the entryless-hit arm above applies. This arm is where EVERY call lands
    // for a receiver class other than the one the MIC was populated with (the
    // slot is monomorphic for its lifetime, `JitMICSlot::update`), so a callee
    // the compiler declined for that class re-ran the probe, its argument pins
    // and the by-name indy probe below on every call, for the life of the
    // site. Keyed and flushed exactly as there: one retry per
    // `MIC_COMPILE_PROBE_RETRY` calls, and every generation / class-identity
    // flush drops the memo with the other site-keyed memos.
    let miss_declined_key = (
        jit_site_key(vm.vm_identity, info_ptr as usize),
        receiver_cid,
    );
    let miss_skip_probe =
        MIC_COMPILE_DECLINED.with(|m| match m.borrow_mut().get_mut(&miss_declined_key) {
            Some(n) => {
                *n = n.wrapping_add(1);
                *n % MIC_COMPILE_PROBE_RETRY != 0
            }
            None => false,
        });
    if miss_skip_probe {
        note_cached_entry_arm(&MIC_COMPILE_PROBE_MEMO_SKIPS);
    }
    let compile_res = if miss_skip_probe
        || !direct_virtual_compiled_callee_entry_enabled()
        || compiled_entry_forbidden
        || !cacheable_receiver
    {
        None
    } else {
        let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
        let pins = CompileArgPins::pin_values(vm, thread, &full_args);
        let probed = crate::runtime::interpreter::try_jit_compile_callee_for_class(
            vm,
            // The receiver's own class, or the §5.4.6-selected one (MEGA17-3).
            ClassId::new(compile_cid),
            &class_name,
            info.method_name,
            info.descriptor,
            // JIT-dispatch callee compile — optimized (C2-equivalent) tier.
            true,
        );
        pins.release_values(thread, &mut full_args);
        // JIT r20 w19 yLadder (DISP20-7): an inherited method's body published
        // under its DECLARING class, which the receiver-keyed probe cannot see;
        // published and called below exactly as a probe answer.
        let probed = probed.or_else(|| probe_selected_declaring_body(vm, info, receiver_class_id));
        // As the entryless-hit arm: record a refusal, drop one that no longer
        // holds.
        if probed.is_none() {
            MIC_COMPILE_DECLINED.with(|m| {
                m.borrow_mut().entry(miss_declined_key).or_insert(0);
            });
        } else {
            MIC_COMPILE_DECLINED.with(|m| {
                m.borrow_mut().remove(&miss_declined_key);
            });
        }
        probed
    };
    // `full_args[0]` is the receiver, re-read from its pin if the probe ran.
    let receiver_ref = match full_args.first() {
        Some(Value::Object(Some(receiver))) => *receiver,
        _ => receiver_ref,
    };
    // `_callee_pin` must outlive the `mic.update` / `pic.install` below — see
    // the matching note in the cache-hit branch.
    let (_callee_pin, entry_ptr, needs_ctx) = match compile_res {
        Some((pin, ptr, nc)) => (Some(pin), ptr as u64, nc),
        None => (None, 0, false),
    };

    // A machine-code MIC/PIC call has no interpreter boundary at which the
    // callee's local handler can be resumed. Leave such callees on the checked
    // helper path; ordinary handler-free callees retain the raw-entry fast path.
    let callee_barred_by_table = !mic_publish_exception_table_callees()
        && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info);
    // jit-invokedynamic-groovy-regression fix — see the matching gate in the
    // cache-hit branch above: an indy-trap-bearing artifact must stay on the
    // dispatch helper, never in a machine-called MIC/PIC entry. Read off the
    // artifact being published when there is one (the name-keyed probe reads
    // another loader's row for a class name that is not globally unique).
    //
    // Round 11 wave 18 (lane hashmap): with no body the answer can reach only
    // the class-only `mic.update` below (entry 0: `publish_mic_rust_cached_entry`
    // and `pic.install` both do nothing for it), and that update changes
    // nothing unless the slot is still empty -- a slot holding another class
    // is monomorphic for its lifetime and `update` returns at once. So the
    // by-name probe (a class-manager read lock, a JIT-cache read lock and a
    // string-keyed lookup) is asked only while the slot is empty, instead of on
    // every call from every receiver class the slot does not hold.
    let callee_has_indy_trap = match _callee_pin.as_ref() {
        Some(pin) => pin.has_indy_trap,
        None if !JitMICSlot::is_empty_class_id(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Acquire),
        ) =>
        {
            false
        }
        None => compiled_entry_has_indy_trap(vm, &class_name, info.method_name, info.descriptor),
    };
    if callee_barred_by_table && !callee_has_indy_trap && cacheable_receiver {
        publish_mic_rust_cached_entry(
            vm.vm_identity,
            info_ptr,
            receiver_cid,
            entry_ptr as usize,
            needs_ctx,
            true,
        );
    }
    if cacheable_receiver && !callee_barred_by_table && !callee_has_indy_trap {
        mark_site_rerun_pins_before_publish(vm, mic, receiver_cid, info, entry_ptr != 0);
        // t1 wave 4 (lane Y2): a refusal here reaches this VM's report.
        let _jdk_only_scope =
            jdk_only_ic_publication_scope(vm, crate::vm::dispatch_policy(vm).is_jdk_only());
        // Update all MIC fields atomically (needs_ctx must match compiled entry ABI)
        // MEGA5-18-4: a gate-first megamorphic site never reads its MIC.
        if !mic_publication_skipped(pic_ptr) {
            mic.update(
                receiver_cid,
                &class_name,
                entry_ptr,
                needs_ctx,
                crate::vm::dispatch_policy(vm).is_jdk_only(),
            );
        }

        // CRIT-1 — Populate the co-allocated PIC so the inline 4-way
        // cascade emitted in `jit/src/x64.rs` actually hits on subsequent
        // dispatches. Eager allocation made `pic_inline` always-true at
        // codegen, so the cascade is always emitted but stays cold until
        // the helper publishes entries here. Mirror the MIC update with
        // a `pic.install(...)` so the next call with the same receiver
        // class takes the inline fast path (5 cycles slot-0 hit vs the
        // full helper call). LFU eviction inside `install` handles
        // megamorphic spillover automatically.
        if pic_ptr != 0 && entry_ptr != 0 {
            let pic = &*(pic_ptr as *const JitPICSlot);
            // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`): as
            // the probe-hit publication above.
            if !pic_way_redundant_with_mic(mic, pic, receiver_cid)
                && pic.install(
                    receiver_cid,
                    &class_name,
                    entry_ptr,
                    needs_ctx,
                    crate::vm::dispatch_policy(vm).is_jdk_only(),
                )
            {
                disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
                request_ic_grace_if_due(vm, Some(thread.thread_id));
            }
            // Round 12 wave 2 (lane calls, W2-1): and the VM's shared
            // megamorphic table, for every other site of this method.
            publish_mega_dispatch_entry(
                vm,
                info,
                pic,
                receiver_cid,
                entry_ptr,
                needs_ctx,
                epoch_now,
            );
        }
        // Round 11 wave 18 (lane hashmap): a receiver class that got a body but
        // that NEITHER cache this helper consults can now serve. The MIC holds
        // another class for its lifetime, and the PIC's hashed table (the only
        // part of the PIC this helper reads; its four inline ways are read by
        // machine code only, and not at all at a single-pass site too wide for
        // the register file) had no free way, or there is no PIC. Every later
        // call from this class used to land back here and pay the full
        // resolution: argument decode, compile probe (a JIT-cache hit under
        // two read locks) and a by-name `invoke_or_native`, per call, while a
        // compiled body sat in the JIT cache. It now goes to the per-thread
        // entry cache the hoisted `try_mic_rust_cached_entry` consult reads,
        // under the same conditions the table-barred publication above meets:
        // a receiver-derived target compiled for this receiver class id.
        // `rerun_pins` is the site's sticky flag, set before any table callee
        // is published here (`mark_site_rerun_pins_before_publish` above).
        if entry_ptr != 0
            && vm.jit.jit_cache.redefine_epoch() == epoch_now
            && mic
                .cached_class_id
                .load(std::sync::atomic::Ordering::Acquire)
                != receiver_cid
            && (pic_ptr == 0
                || (*(pic_ptr as *const JitPICSlot))
                    .lookup_megamorphic(receiver_cid)
                    .is_none())
        {
            publish_mic_rust_cached_entry(
                vm.vm_identity,
                info_ptr,
                receiver_cid,
                entry_ptr as usize,
                needs_ctx,
                mic.callee_rerun_pins(),
            );
        }
        withdraw_ic_publication_if_redefined(
            vm,
            epoch_now,
            mic,
            (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot)),
        );
    }

    // Round 12 wave 1 (lane `calls`, W18-3): the probe just answered a body
    // for this receiver's class; enter it rather than re-resolving by name.
    if entry_ptr != 0
        && cacheable_receiver
        && receiver_is_plain_object
        && !callee_has_indy_trap
        && !compiled_entry_forbidden
    {
        if let Some(rc) = call_body_the_mic_probe_just_found(
            vm,
            thread,
            info,
            receiver_class_id,
            entry_ptr as usize,
            needs_ctx,
            vm_ptr,
            &full_args[..],
            // gch h1/jp: as the entryless-hit arm.
            forwarded_args.is_none().then_some(args_slice),
            callee_barred_by_table || mic.callee_rerun_pins(),
            epoch_now,
        ) {
            disp_census::note(disp_census::MIC_MISS_CALLED_COMPILED);
            return rc;
        }
    }

    // `full_args` was decoded above, ahead of the compile probe.

    // The uncompiled-callee fast path — same gate and same reasoning as the
    // cache-hit branch above: `entry_ptr == 0` means the compile probe found no
    // compiled body for this callee, so nothing is being diverted away from one.
    if entry_ptr == 0 {
        if let Some(r) = try_jit_virtual_bytecode_callee(
            vm,
            thread,
            info,
            jit_site_key(vm.vm_identity, info_ptr as usize),
            receiver_ref,
            receiver_class_id,
            &full_args,
            false,
        ) {
            return match r {
                Ok(Some(Value::Int(v))) => v as i64,
                Ok(Some(Value::Long(v))) => v,
                Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(Some(Value::Object(None)) | None) => 0,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // gci i12/mf: as the entryless-hit arm's tail.
    let jni_vsite_probe =
        crate::vm::vm_exec::jni_jit_vsite_probe_arm(vm, thread, info.method_name, info.descriptor);
    let invoke_res = {
        let _g = mic_prof::CycGuard::new(&mic_prof::CYC_INVOKE);
        crate::vm::dbg_dispatch_tally(
            "jit_mic_tail",
            &class_name,
            info.method_name,
            info.descriptor,
        );
        // gch h1/jp (fr review item 3): as the entryless-hit arm's tail.
        crate::vm::invoke_or_native_keeping_args_current(
            vm,
            thread,
            &class_name,
            info.method_name,
            info.descriptor,
            &mut full_args[..],
            !info.class_name.is_empty() && &*class_name != info.class_name,
        )
    };
    if let Some(save) = jni_vsite_probe {
        crate::vm::vm_exec::jni_jit_vsite_probe_finish(
            vm,
            thread,
            save,
            ClassId::new(info.declaring_class_id),
            crate::vm::vm_exec::jni_jit_vsite(info_ptr as usize),
            receiver_class_id,
            &class_name,
            info.class_name,
            info.method_name,
            info.descriptor,
            invoke_res.is_ok() && jni_vsite_fill_ok && receiver_is_plain_object,
        );
    }
    // S111r12 — JIT MIC virtual-dispatch rescue. When the receiver's
    // runtime class (e.g. `java/lang/Comparable` for a malformed
    // ClassLoader instance) does not declare the CP-resolved method,
    // `invoke_or_native` raises `NoSuchMethodError`. The CP method-ref
    // class carried in `info.class_name` (e.g. `java/lang/ClassLoader`)
    // is the spec-correct resolution target — retry through it.
    // Mirrors the S111r10 receiver-walk fallback for invokeinterface
    // and the S111r8 cid=0 → CP-class fallback in `execute_invoke`.
    let result = match invoke_res {
        Ok(v) => v,
        Err(e)
            if !info.class_name.is_empty()
                && &*class_name != info.class_name
                && is_dispatch_no_such_method_miss(
                    &e,
                    &class_name,
                    info.method_name,
                    info.descriptor,
                ) =>
        {
            match crate::vm::invoke_or_native(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                &full_args,
            ) {
                Ok(v) => v,
                Err(e2) => {
                    return handle_jit_dispatch_error(vm, thread, e2, info);
                }
            }
        }
        // Round-fix (Jetty): route a thrown exception through
        // `handle_jit_dispatch_error` (stash + return the `i64::MIN` deopt
        // sentinel) rather than logging and returning a bogus 0.
        Err(e) => {
            return handle_jit_dispatch_error(vm, thread, e, info);
        }
    };

    if cv_trace {
        eprintln!(
            "[cv-mic-miss-result] is_null={}",
            matches!(result, Some(Value::Object(None)) | None)
        );
    }
    match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Uncommon Trap / Deoptimization
// ---------------------------------------------------------------------------

/// Deopt reason codes passed from JIT-compiled code.
/// These map to `cratonvm_jit::deopt::DeoptReason` variants.
pub const DEOPT_REASON_NULL_CHECK: i64 = 0;

/// JIT round 21, lane tlsub (TL321-4): the layout producer's one-class query
/// shares the helper's map-kind encoding, and declines -- writing nothing --
/// without a VM, under `--compatible`, and before the `ThreadLocal` classes
/// are loaded.
#[cfg(test)]
mod jr21_tlsub_receiver_query_tests {
    use super::*;

    fn ask(vm: Option<&SharedVm>, class_id: u32) -> (i64, [u64; 2]) {
        let mut io = [u64::from(class_id), 7u64];
        let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
        let got = unsafe {
            jit_thread_local_ir_layout(
                vm_ptr,
                io.as_mut_ptr() as i64,
                cratonvm_jit::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64,
            )
        };
        (got, io)
    }

    #[test]
    fn jr21_tlsub_kinds_are_the_helpers_own() {
        assert_eq!(
            TL_MAP_PLAIN,
            cratonvm_jit::direct_helpers::TL_RECEIVER_KIND_PLAIN
        );
        assert_eq!(
            TL_MAP_INHERITABLE,
            cratonvm_jit::direct_helpers::TL_RECEIVER_KIND_INHERITABLE
        );
        assert_eq!(
            cratonvm_jit::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS,
            2
        );
    }

    #[test]
    fn jr21_tlsub_declines_without_a_write() {
        assert_eq!(ask(None, 42), (0, [42, 7]));
        let compatible = SharedVm::new(crate::config::VmConfig::default());
        assert_eq!(ask(Some(&compatible), 42), (0, [42, 7]));
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        config.use_synthetic_jdk = false;
        let jdk_only = SharedVm::new(config);
        // A fresh VM has not loaded the `ThreadLocal` classes: no layout.
        assert_eq!(ask(Some(&jdk_only), 42), (0, [42, 7]));
    }
}

/// j27 wave 3 (lane BACKLOG, STR16-P7): a text pin for the `DecimalDigits`
/// thin helpers (they cannot be driven without compiled code). Every compiled
/// `int`/`long` -> decimal conversion runs this body, so it must not write a
/// process-wide counter per call: a shared cache line every converting thread
/// bounces, for a census nothing read.
#[cfg(test)]
mod j27_backlog_decimal_digits_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    #[test]
    fn the_decimal_digits_helper_body_counts_nothing_per_call() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn jit_decimal_digits_latin1_direct_body(");
        assert!(body.contains("decimal_digits_latin1_serve(vm, value, index, buf)"));
        assert!(body.contains("jit_invoke_dispatch("), "the cold arm still runs the JDK body");
        assert!(!body.contains("fetch_add"), "no per-call counter on the hot path");
        assert!(!body.contains("DECIMAL_DIGITS_CENSUS"), "the census counts sites only");
    }
}
