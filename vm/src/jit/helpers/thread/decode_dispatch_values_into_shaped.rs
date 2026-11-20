#[allow(unused_imports)]
use super::*;

/// [`decode_dispatch_values_into`] for a caller holding the site's parameter
/// tags already — see [`NativeSiteDescriptor`]. `params` is `None` for a
/// caller that has none, and then the descriptor is parsed exactly as before.
///
/// `prevalidated` is [`forward_jit_reference_args_validated`]'s mask for this
/// very buffer (`0` = none): a parameter word whose bit is set is taken as the
/// validated reference it is, without a second heap walk (FUNNEL17-2, JIT
/// round 19 wave 2, lane funnel2).
#[inline]
pub unsafe fn decode_dispatch_values_into_shaped(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver: Option<Option<ObjectRef>>,
    params: Option<&[u8]>,
    prevalidated: u64,
    values: &mut JitDecodedArgs,
) {
    values.clear();
    let mut desc_iter = DescriptorParamIter::new(info.descriptor);
    let mut param_index = 0usize;

    if jit_info_has_receiver(info) {
        if !args_slice.is_empty() {
            match receiver {
                // The caller already ran the validator on this very slot.
                Some(resolved) => values.push(Value::Object(resolved)),
                None => {
                    let ptr = args_slice[0];
                    if ptr == 0 {
                        values.push(Value::Object(None));
                    } else {
                        // Defensive: tagged-long bits in an L-typed receiver slot
                        // are downgraded to null instead of being treated as a
                        // heap pointer (else GC SEGVs walking a bogus oop).
                        let bits = ptr as u64;
                        let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                            crate::jit::helpers::note_membership_walk(2);
                            vm.mem.heap.is_object_address(bits as usize)
                        } else {
                            None
                        };
                        match validated {
                            Some(obj) => values.push(Value::Object(Some(obj))),
                            None => values.push(Value::Object(None)),
                        }
                    }
                }
            }
        }
    }

    let start_idx = usize::from(jit_info_has_receiver(info));
    for (offset, &raw) in args_slice[start_idx..].iter().enumerate() {
        let tag = match params {
            Some(tags) => {
                let tag = tags.get(param_index).copied();
                param_index += 1;
                tag
            }
            None => desc_iter.next(),
        };
        let val = match tag {
            Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                Value::Int(raw as i32)
            }
            Some(b'J') => Value::Long(raw),
            Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
            Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
            Some(b'L') | Some(b'[') => {
                if raw == 0 {
                    Value::Object(None)
                } else if let Some(obj) =
                    prevalidated_jit_ref(prevalidated, start_idx + offset, raw)
                {
                    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`): the
                    // forwarding pass validated this word; no second walk.
                    Value::Object(Some(obj))
                } else {
                    let bits = raw as u64;
                    let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                        crate::jit::helpers::note_membership_walk(2);
                        vm.mem.heap.is_object_address(bits as usize)
                    } else {
                        None
                    };
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
}

// ---------------------------------------------------------------------------
// Helper: extract heap from SharedVm pointer
// ---------------------------------------------------------------------------

// SAFETY: Caller must ensure vm_ptr is a valid pointer to a live SharedVm instance.
// The SharedVm is heap-allocated and outlives all JIT helper calls.
#[inline]
pub unsafe fn heap_from_vm(vm_ptr: i64) -> &'static VmHeap {
    debug_assert!(vm_ptr != 0, "heap_from_vm called with null VM pointer");
    // SAFETY: vm_ptr was passed from JIT-compiled code which received it from the
    // interpreter's SharedVm reference, so it points to a valid SharedVm.
    &(*(vm_ptr as *const SharedVm)).mem.heap
}

// ---------------------------------------------------------------------------
// JIT safepoint SATB flush — Round-7 fix (CRIT, audit §3)
// ---------------------------------------------------------------------------
//
// The interpreter drains its per-thread SATB buffer at every safepoint
// arrival (`runtime/interpreter.rs::safepoint_check`, line 899). JIT-
// running threads have no such poll — they only return through one of
// the runtime helpers below. If any of those helpers participates in an
// STW pause (directly via `collect_garbage` or transitively via the
// shared barrier on a concurrent GC trigger) without first draining the
// per-thread SATB buffer, up to `DEFAULT_SATB_CAPACITY` (256) overwritten
// references stay invisible to the marker. The next mixed evacuation
// then turns the classic SATB lost-object scenario into a use-after-
// free (audit: round7-gc.md §3).
//
// `flush_thread_satb` itself is a cheap inline call when `is_active() ==
// false`: a single Acquire load and an early return. We invoke it
// unconditionally at the top of every GC-triggering JIT helper so the
// invariant holds without a separate JIT-emitted safepoint stub.
//
// The same door retires a leftover native-return handoff root first —
// `drain_native_return_at_compiled_helper_entry`, r11 2026-09-25: compiled
// code never consumes `native_pending_return`, so without it the LAST object a
// compiled-code native call returned (a `SoftReference.get()` referent, say)
// stayed a GC root through every collection this helper went on to run.
// SAFETY: every caller is a JIT helper invoked from compiled code; `vm_ptr` is
// either 0 (handled by the early return) or a live `SharedVm` pointer per the
// universal JIT-helper caller contract.
#[inline]
pub unsafe fn jit_safepoint_flush_satb(vm_ptr: i64) {
    drain_native_return_at_compiled_helper_entry();
    if vm_ptr == 0 {
        return;
    }
    // SAFETY: caller contract for every JIT helper — vm_ptr is a live
    // SharedVm pointer.
    let vm = &*(vm_ptr as *const SharedVm);
    vm.mem.heap.flush_thread_satb();
    serve_handshakes_at_helper_entry(vm);
}

/// JIT round 23 wave 6 (`CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY`, default ON,
/// `=0` kills; the proxy-define page): run this thread's queued per-thread
/// handshakes at a helper's entry door. Compiled code reaches
/// `safepoint_check` only at its poll's slow path, which fires on a pause
/// request alone, or at an allocation slow path, so a compiled peer left a
/// cross-thread frame-trace handshake unserved for the whole 1 ms budget and
/// the requester then stopped the world anyway. A helper entry is a point
/// where this thread can walk its own frames: the same state as a native it
/// is about to dispatch to (`Throwable.fillInStackTrace`), whose own-thread
/// walk reads the same compiled chain and the call site's stamp. The op runs
/// ON this thread, so no other thread walks frames that can move.
/// Steady state: one TLS load and one `Acquire` load.
#[inline]
pub unsafe fn serve_handshakes_at_helper_entry(vm: &SharedVm) {
    if !cratonvm_types::flags().gc.handshake_at_helper_entry {
        return;
    }
    if let Some((thread, _guard)) = jit_thread_mut() {
        if thread.gc_block_state.handshakes.is_pending() {
            thread.run_pending_handshakes(vm);
        }
    }
}

