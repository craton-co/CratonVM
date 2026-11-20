use super::*;

/// The scope nests like the guards: an unscoped (process-wide) proof inside
/// a scoped one covers everything, and the scoped one is back afterwards.
#[test]
fn the_reclaim_scope_nests_and_restores() {
    assert!(!reclaim_is_cascaded(0x1000), "no reclamation, no cascade");
    {
        let _outer = AuthorisedReclaim::enter_for(0x1000);
        assert!(reclaim_is_authorised());
        assert!(!reclaim_is_cascaded(0x1000), "the proven buffer itself");
        assert!(reclaim_is_cascaded(0x2000), "any other buffer is a cascade");
        {
            let _inner = AuthorisedReclaim::enter();
            assert!(
                !reclaim_is_cascaded(0x2000),
                "a process-wide proof covers every buffer"
            );
        }
        assert!(reclaim_is_cascaded(0x2000), "the scoped proof is restored");
    }
    assert!(!reclaim_is_authorised());
    assert!(!reclaim_is_cascaded(0x2000));
}

/// The `gcd-d10v` shape: a body freed on per-thread evidence drops the last
/// owner of ANOTHER published body. That body must not be unmapped on the
/// first body's proof -- it is re-queued, so it stays mapped while this
/// thread is inside compiled code. Before the fix the drop unmapped it at
/// once (any authorised depth authorised every nested drop).
#[test]
fn a_published_body_released_by_another_bodys_reclamation_stays_mapped() {
    // Hold this thread inside JIT execution, so the re-queued mapping
    // cannot be released by any drain until the token is returned.
    let token = jit_execution_enter();
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    buf.mark_published();
    buf.finalize();
    let base = buf.as_ptr() as usize;
    {
        // Another body's proof: a buffer start that is not this one.
        let _other = AuthorisedReclaim::enter_for(base.wrapping_add(0x10_0000));
        drop(buf);
    }
    assert!(
        validate_code_ptr(base as *const u8).is_ok(),
        "a cascaded published body must stay mapped and registered until \
         the queue proves it on its own"
    );
    jit_execution_leave(token);
}

extern "C" fn not_compiled_code() -> i64 {
    7
}

/// i74-L2: a live frame reports the method's holder when the publishing
/// door stamped one (an inherited body under its receiver's key), and the
/// publication owner otherwise.
#[test]
fn a_frame_reports_the_holder_class_over_the_receiver_key_owner() {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    assert_eq!(
        cm.holder_class_id,
        cratonvm_types::jit_activation::NO_OWNER_CLASS
    );
    cm.owner_class_id = 41;
    assert_eq!(cm.frame_class_id(), 41, "no holder stamped: the owner");
    cm.holder_class_id = 0;
    assert_eq!(cm.frame_class_id(), 41, "a zero holder names no class");
    cm.holder_class_id = 17;
    assert_eq!(cm.frame_class_id(), 17, "the declaring class wins");
    assert_eq!(cm.owner_class_id, 41, "the key owner is untouched");
}

/// `r14w1-codecache-unpinned-raw-entry-call-patch`: a registered range
/// whose owner cannot be retained is DEAD compiled code (never entered),
/// while an address outside every JIT range and region -- a Rust function
/// -- is NOT JIT code and may be called as before.
#[test]
fn a_checked_pin_tells_dead_compiled_code_from_non_jit_code() {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let cm = CompiledMethod::new(buf);
    let entry = cm.entry_ptr() as usize;
    // Non-owning registration: the range names a body nothing can retain,
    // the state a body is in between its last `Arc` drop and its `Drop`.
    register_jit_code_range(entry, cm.code_len().max(1), &cm as *const _ as usize);
    assert!(matches!(
        pin_jit_code_range_owner_checked(entry),
        JitEntryPin::Dead
    ));
    drop(cm);
    let rust_fn = not_compiled_code as extern "C" fn() -> i64 as usize;
    assert!(matches!(
        pin_jit_code_range_owner_checked(rust_fn),
        JitEntryPin::NotJit
    ));
}

/// Round 14 wave 2: the envelope of every registered buffer covers each
/// live buffer, and an address outside it is `NotJit` from the envelope
/// alone (the arm that takes no lock and walks no ring).
#[test]
fn an_address_outside_every_mapped_buffer_is_not_jit_without_a_lookup() {
    use std::sync::atomic::Ordering::Relaxed;
    let buf = ExecutableBuffer::new(64).expect("executable buffer");
    let base = buf.as_ptr() as usize;
    let (lo, hi) = (
        RECLAMATION.mapped_lo.load(Relaxed),
        RECLAMATION.mapped_hi.load(Relaxed),
    );
    assert!(
        lo <= base && base < hi,
        "a registered buffer is inside the envelope"
    );
    // Page zero is never an executable buffer, and nothing maps the last page.
    for addr in [0x1000usize, usize::MAX - 0xFFF] {
        assert!(addr < lo || addr >= hi, "{addr:#x} is outside the envelope");
        assert!(matches!(
            pin_jit_code_range_owner_checked(addr),
            JitEntryPin::NotJit
        ));
    }
    drop(buf);
}
